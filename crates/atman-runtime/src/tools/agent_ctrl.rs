use crate::error::RuntimeError;
use crate::event::{Event, FlowRunId, FlowStatus};
use crate::git_workspace::{
    WorkspaceBinding, WorkspaceFinalizeOutcome, WorkspacePolicy, WorkspaceState,
};
use crate::message::Message;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SPAWN_PERMIT_TTL: Duration = Duration::from_secs(300);

pub struct AgentSpawn;

#[derive(Debug, Clone)]
pub enum FlowRunStatus {
    Running {
        started_at: chrono::DateTime<chrono::Utc>,
    },
    Ok {
        ended_at: chrono::DateTime<chrono::Utc>,
        final_text: String,
    },
    Err {
        ended_at: chrono::DateTime<chrono::Utc>,
        message: String,
    },
    Killed {
        ended_at: chrono::DateTime<chrono::Utc>,
    },
}

impl FlowRunStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Running { .. } => "running",
            Self::Ok { .. } => "ok",
            Self::Err { .. } => "err",
            Self::Killed { .. } => "killed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum FlowEvent {
    AssistantDone { text: String },
    Exited { status: FlowRunStatus },
}

pub struct FlowEntry {
    pub handle: String,
    pub goal: String,
    pub display_label: String,
    pub status: Arc<Mutex<FlowRunStatus>>,
    pub output: Arc<Mutex<String>>,
    pub cancel: tokio_util::sync::CancellationToken,
    pub stream_tx: tokio::sync::broadcast::Sender<FlowEvent>,
    pub messages: Arc<Mutex<Vec<Message>>>,
    pub iteration: Arc<std::sync::atomic::AtomicU64>,
    pub child_run_id: FlowRunId,
    pub model: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub compact_lock: Arc<tokio::sync::Mutex<()>>,
    pub interjection_tx: tokio::sync::broadcast::Sender<crate::injection::Injection>,
    pub pending_injections: Arc<std::sync::Mutex<Vec<crate::injection::Injection>>>,
    pub injection_notify: Arc<tokio::sync::Notify>,
    pub frame_tx: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
    pub workspace: Option<WorkspaceBinding>,
    pub workspace_state: Arc<Mutex<Option<WorkspaceState>>>,
    pub cleanup_error: Arc<Mutex<Option<String>>>,
}

impl crate::watch::Watchable for FlowEntry {
    fn watch_output(
        self: Arc<Self>,
        pattern: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::watch::WatchResult> + Send>>
    {
        let stream_tx = self.stream_tx.clone();
        let output = self.output.clone();
        let status = self.status.clone();
        Box::pin(async move {
            {
                let existing = output.lock().unwrap().clone();
                if existing.find(&pattern).is_some() {
                    return crate::watch::WatchResult::Matched {
                        row: None,
                        col: None,
                        text: existing,
                    };
                }
            }
            // Subscribe BEFORE checking status — if the source exits between
            // subscribe and the status check, the broadcast event arrives via rx.
            // If it exits before subscribe, the status check catches it.
            let mut rx = stream_tx.subscribe();
            {
                let st = status.lock().unwrap().clone();
                if !st.is_running() {
                    if let FlowRunStatus::Ok { final_text, .. } = &st {
                        if final_text.find(&pattern).is_some() {
                            return crate::watch::WatchResult::Matched {
                                row: None,
                                col: None,
                                text: final_text.clone(),
                            };
                        }
                    }
                    return crate::watch::WatchResult::SourceExited;
                }
            }
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return crate::watch::WatchResult::Cancelled,
                    result = rx.recv() => match result {
                        Ok(FlowEvent::AssistantDone { text }) => {
                            if text.find(&pattern).is_some() {
                                return crate::watch::WatchResult::Matched {
                                    row: None,
                                    col: None,
                                    text,
                                };
                            }
                        }
                        Ok(FlowEvent::Exited { status }) => {
                            if let FlowRunStatus::Ok { final_text, .. } = &status {
                                if final_text.find(&pattern).is_some() {
                                    return crate::watch::WatchResult::Matched {
                                        row: None,
                                        col: None,
                                        text: final_text.clone(),
                                    };
                                }
                            }
                            return crate::watch::WatchResult::SourceExited;
                        }
                        Err(_) => return crate::watch::WatchResult::SourceExited,
                    }
                }
            }
        })
    }
}

/// Observes flow terminal transitions while the lifecycle arbitration is held, so
/// subsystems keyed by run liveness can be updated inside the same linearization point.
pub(crate) trait FlowTerminalObserver: Send + Sync {
    fn flow_became_terminal(
        &self,
        session_id: &str,
        run_id: &FlowRunId,
    ) -> Option<Box<dyn FnOnce() + Send>>;
}

#[derive(Default)]
pub struct FlowRegistry {
    entries: Mutex<std::collections::HashMap<String, Arc<FlowEntry>>>,
    runs: Mutex<std::collections::HashMap<FlowRunId, Arc<crate::flow_authority::FlowIdentity>>>,
    /// Serializes every identity/execution-state transition. Held around observer
    /// notification so a run cannot go terminal between a liveness check and a
    /// decision commit in another subsystem. Lock order: lifecycle -> runs -> identity.
    lifecycle: Mutex<()>,
    terminal_observers: Mutex<Vec<std::sync::Weak<dyn FlowTerminalObserver>>>,
    spawn_admission: Mutex<SpawnAdmission>,
}

#[derive(Default)]
struct SpawnAdmission {
    revisions: std::collections::HashMap<String, u64>,
    permits: std::collections::HashMap<String, SpawnPermit>,
}

struct SpawnPermit {
    session_id: String,
    parent_run_id: FlowRunId,
    revision: u64,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct FlowInstanceInfo {
    pub handle: String,
    pub goal: String,
    pub model: String,
    pub status: String,
    pub child_run_id: FlowRunId,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

pub struct DescendantBlockGuard {
    registry: Arc<FlowRegistry>,
    parent_run_id: FlowRunId,
    child_run_id: FlowRunId,
}

pub struct FlowLifecycleGuard {
    registry: Arc<FlowRegistry>,
    run_id: FlowRunId,
}

impl Drop for DescendantBlockGuard {
    fn drop(&mut self) {
        self.registry
            .unblock_descendant(&self.parent_run_id, &self.child_run_id);
    }
}

impl Drop for FlowLifecycleGuard {
    fn drop(&mut self) {
        self.registry.mark_terminal(&self.run_id);
    }
}

impl FlowRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump_spawn_inventory(&self, session_id: &str) {
        let mut admission = self.spawn_admission.lock().unwrap();
        let revision = admission
            .revisions
            .entry(session_id.to_owned())
            .or_default();
        *revision = revision.wrapping_add(1);
        admission
            .permits
            .retain(|_, permit| permit.session_id != session_id);
    }

    pub fn issue_spawn_permit(&self, identity: &crate::flow_authority::FlowIdentity) -> String {
        let mut admission = self.spawn_admission.lock().unwrap();
        Self::issue_spawn_permit_locked(&mut admission, identity)
    }

    fn issue_spawn_permit_locked(
        admission: &mut SpawnAdmission,
        identity: &crate::flow_authority::FlowIdentity,
    ) -> String {
        let now = Instant::now();
        admission
            .permits
            .retain(|_, permit| permit.expires_at > now);
        let revision = admission
            .revisions
            .get(&identity.session_id)
            .copied()
            .unwrap_or_default();
        let token = format!("spawn_{}", uuid::Uuid::now_v7().simple());
        admission.permits.insert(
            token.clone(),
            SpawnPermit {
                session_id: identity.session_id.clone(),
                parent_run_id: identity.run_id.clone(),
                revision,
                expires_at: now + SPAWN_PERMIT_TTL,
            },
        );
        token
    }

    pub fn inspect_for_spawn(
        &self,
        identity: &crate::flow_authority::FlowIdentity,
    ) -> (Vec<FlowInstanceInfo>, String) {
        let mut admission = self.spawn_admission.lock().unwrap();
        let instances = self.instances_for_session(&identity.session_id);
        let token = Self::issue_spawn_permit_locked(&mut admission, identity);
        (instances, token)
    }

    pub fn consume_spawn_permit(
        &self,
        token: &str,
        identity: &crate::flow_authority::FlowIdentity,
    ) -> Result<(), RuntimeError> {
        let mut admission = self.spawn_admission.lock().unwrap();
        let Some(permit) = admission.permits.remove(token) else {
            return Err(RuntimeError::ToolFailed(
                "flow.spawn: missing, expired, or already used spawn_token; call flow.instances, inspect existing flows, reuse suitable work, and kill obsolete flows before spawning"
                    .into(),
            ));
        };
        let current_revision = admission
            .revisions
            .get(&identity.session_id)
            .copied()
            .unwrap_or_default();
        if permit.expires_at <= Instant::now()
            || permit.session_id != identity.session_id
            || permit.parent_run_id != identity.run_id
        {
            return Err(RuntimeError::ToolFailed(
                "flow.spawn: spawn_token does not belong to this caller or has expired; call flow.instances again"
                    .into(),
            ));
        }
        if permit.revision != current_revision {
            return Err(RuntimeError::ToolFailed(
                "flow.spawn: flow inventory changed after inspection; call flow.instances again before spawning"
                    .into(),
            ));
        }
        let revision = admission
            .revisions
            .entry(identity.session_id.clone())
            .or_default();
        *revision = revision.wrapping_add(1);
        admission
            .permits
            .retain(|_, permit| permit.session_id != identity.session_id);
        Ok(())
    }

    pub fn instances_for_session(&self, session_id: &str) -> Vec<FlowInstanceInfo> {
        let runs = self.runs.lock().unwrap();
        let entries = self.entries.lock().unwrap();
        let mut instances = entries
            .values()
            .filter(|entry| {
                runs.get(&entry.child_run_id)
                    .is_some_and(|identity| identity.session_id == session_id)
            })
            .map(|entry| FlowInstanceInfo {
                handle: entry.handle.clone(),
                goal: entry.display_label.clone(),
                model: entry.model.clone(),
                status: entry.status.lock().unwrap().kind_str().to_owned(),
                child_run_id: entry.child_run_id.clone(),
                started_at: entry.started_at,
            })
            .collect::<Vec<_>>();
        instances.sort_by_key(|entry| entry.started_at);
        instances
    }

    /// Runs `f` under the lifecycle arbitration. Callers must not already hold it,
    /// and must not acquire it again from inside `f`.
    pub(crate) fn with_lifecycle_arbitration<T>(&self, f: impl FnOnce() -> T) -> T {
        let _lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    pub(crate) fn register_terminal_observer(
        &self,
        observer: std::sync::Weak<dyn FlowTerminalObserver>,
    ) {
        let mut observers = self.terminal_observers.lock().unwrap();
        observers.retain(|existing| existing.strong_count() > 0);
        observers.push(observer);
    }

    pub fn register_root(
        &self,
        session_id: String,
        run_id: FlowRunId,
        authority: crate::flow_authority::EffectiveAuthority,
    ) -> Result<Arc<crate::flow_authority::FlowIdentity>, RuntimeError> {
        self.with_lifecycle_arbitration(|| self.register_root_locked(session_id, run_id, authority))
    }

    fn register_root_locked(
        &self,
        session_id: String,
        run_id: FlowRunId,
        authority: crate::flow_authority::EffectiveAuthority,
    ) -> Result<Arc<crate::flow_authority::FlowIdentity>, RuntimeError> {
        let identity = Arc::new(crate::flow_authority::FlowIdentity {
            session_id,
            run_id: run_id.clone(),
            parent_run_id: None,
            root_run_id: run_id.clone(),
            invocation: crate::flow_authority::InvocationKind::Root,
            effective_authority: authority,
            execution_state: Mutex::new(crate::flow_authority::FlowExecutionState::Running),
        });
        let mut runs = self.runs.lock().unwrap();
        if runs.contains_key(&run_id) {
            return Err(RuntimeError::ToolFailed(format!(
                "flow identity '{run_id}' is already registered"
            )));
        }
        runs.insert(run_id, Arc::clone(&identity));
        Ok(identity)
    }

    pub fn register_child(
        &self,
        parent_run_id: &FlowRunId,
        child_run_id: FlowRunId,
        invocation: crate::flow_authority::InvocationKind,
        contract_allows_shell: bool,
        workspace: crate::flow_authority::ChildWorkspaceAuthority,
    ) -> Result<Arc<crate::flow_authority::FlowIdentity>, RuntimeError> {
        self.with_lifecycle_arbitration(|| {
            self.register_child_locked(
                parent_run_id,
                child_run_id,
                invocation,
                contract_allows_shell,
                workspace,
            )
        })
    }

    fn register_child_locked(
        &self,
        parent_run_id: &FlowRunId,
        child_run_id: FlowRunId,
        invocation: crate::flow_authority::InvocationKind,
        contract_allows_shell: bool,
        workspace: crate::flow_authority::ChildWorkspaceAuthority,
    ) -> Result<Arc<crate::flow_authority::FlowIdentity>, RuntimeError> {
        if invocation == crate::flow_authority::InvocationKind::Root {
            return Err(RuntimeError::ToolFailed(
                "child flow identity cannot use root invocation".into(),
            ));
        }
        let mut runs = self.runs.lock().unwrap();
        if runs.contains_key(&child_run_id) {
            return Err(RuntimeError::ToolFailed(format!(
                "flow identity '{child_run_id}' is already registered"
            )));
        }
        let parent = runs.get(parent_run_id).cloned().ok_or_else(|| {
            RuntimeError::ToolFailed(format!(
                "parent flow identity '{parent_run_id}' is not registered"
            ))
        })?;
        if matches!(
            parent.execution_state(),
            crate::flow_authority::FlowExecutionState::Terminal
        ) {
            return Err(RuntimeError::ToolFailed(format!(
                "parent flow identity '{parent_run_id}' is terminal"
            )));
        }
        let identity = Arc::new(crate::flow_authority::FlowIdentity {
            session_id: parent.session_id.clone(),
            run_id: child_run_id.clone(),
            parent_run_id: Some(parent_run_id.clone()),
            root_run_id: parent.root_run_id.clone(),
            invocation,
            effective_authority: parent
                .effective_authority
                .inherited_child(contract_allows_shell, workspace)
                .map_err(|error| RuntimeError::ToolFailed(format!("child authority: {error}")))?,
            execution_state: Mutex::new(crate::flow_authority::FlowExecutionState::Running),
        });
        runs.insert(child_run_id, Arc::clone(&identity));
        Ok(identity)
    }

    pub fn lookup_run(
        &self,
        run_id: &FlowRunId,
    ) -> Option<Arc<crate::flow_authority::FlowIdentity>> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }

    pub fn is_strict_ancestor(&self, ancestor: &FlowRunId, descendant: &FlowRunId) -> bool {
        if ancestor == descendant {
            return false;
        }
        let runs = self.runs.lock().unwrap();
        let Some(ancestor_identity) = runs.get(ancestor) else {
            return false;
        };
        let Some(mut current) = runs.get(descendant).cloned() else {
            return false;
        };
        if ancestor_identity.session_id != current.session_id {
            return false;
        }
        let mut visited = std::collections::HashSet::new();
        while let Some(parent_run_id) = current.parent_run_id.as_ref() {
            if !visited.insert(current.run_id.clone()) {
                return false;
            }
            if parent_run_id == ancestor {
                return true;
            }
            let Some(parent) = runs.get(parent_run_id) else {
                return false;
            };
            if parent.session_id != ancestor_identity.session_id {
                return false;
            }
            current = Arc::clone(parent);
        }
        false
    }

    pub fn strict_ancestors(
        &self,
        run_id: &FlowRunId,
    ) -> Vec<Arc<crate::flow_authority::FlowIdentity>> {
        let runs = self.runs.lock().unwrap();
        let Some(start) = runs.get(run_id) else {
            return Vec::new();
        };
        let session_id = start.session_id.clone();
        let mut current = Arc::clone(start);
        let mut ancestors = Vec::new();
        let mut visited = std::collections::HashSet::new();
        while let Some(parent_run_id) = current.parent_run_id.as_ref() {
            if !visited.insert(current.run_id.clone()) {
                return Vec::new();
            }
            let Some(parent) = runs.get(parent_run_id) else {
                return Vec::new();
            };
            if parent.session_id != session_id {
                return Vec::new();
            }
            ancestors.push(Arc::clone(parent));
            current = Arc::clone(parent);
        }
        ancestors
    }

    pub fn execution_state(
        &self,
        run_id: &FlowRunId,
    ) -> Option<crate::flow_authority::FlowExecutionState> {
        self.lookup_run(run_id)
            .map(|identity| identity.execution_state())
    }

    pub fn mark_terminal(&self, run_id: &FlowRunId) {
        let completions = self.with_lifecycle_arbitration(|| self.mark_terminal_locked(run_id));
        for completion in completions {
            completion();
        }
    }

    fn mark_terminal_locked(&self, run_id: &FlowRunId) -> Vec<Box<dyn FnOnce() + Send>> {
        let Some(identity) = self.lookup_run(run_id) else {
            return Vec::new();
        };
        *identity.execution_state.lock().unwrap() =
            crate::flow_authority::FlowExecutionState::Terminal;
        self.bump_spawn_inventory(&identity.session_id);
        let observers: Vec<_> = {
            let mut observers = self.terminal_observers.lock().unwrap();
            observers.retain(|existing| existing.strong_count() > 0);
            observers
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect()
        };
        observers
            .into_iter()
            .filter_map(|observer| observer.flow_became_terminal(&identity.session_id, run_id))
            .collect()
    }

    pub fn lifecycle_guard(self: &Arc<Self>, run_id: &FlowRunId) -> FlowLifecycleGuard {
        FlowLifecycleGuard {
            registry: Arc::clone(self),
            run_id: run_id.clone(),
        }
    }

    pub fn block_on_descendant(
        self: &Arc<Self>,
        parent_run_id: &FlowRunId,
        child_run_id: &FlowRunId,
    ) -> Result<DescendantBlockGuard, RuntimeError> {
        self.with_lifecycle_arbitration(|| {
            self.block_on_descendant_locked(parent_run_id, child_run_id)
        })
    }

    fn block_on_descendant_locked(
        self: &Arc<Self>,
        parent_run_id: &FlowRunId,
        child_run_id: &FlowRunId,
    ) -> Result<DescendantBlockGuard, RuntimeError> {
        if !self.is_strict_ancestor(parent_run_id, child_run_id) {
            return Err(RuntimeError::ToolFailed(format!(
                "flow '{parent_run_id}' is not a strict ancestor of '{child_run_id}'"
            )));
        }
        let parent = self.lookup_run(parent_run_id).ok_or_else(|| {
            RuntimeError::ToolFailed(format!("flow identity '{parent_run_id}' is not registered"))
        })?;
        let mut state = parent.execution_state.lock().unwrap();
        match &mut *state {
            crate::flow_authority::FlowExecutionState::Running => {
                *state = crate::flow_authority::FlowExecutionState::BlockedOnDescendants {
                    child_run_counts: std::collections::HashMap::from([(child_run_id.clone(), 1)]),
                };
            }
            crate::flow_authority::FlowExecutionState::BlockedOnDescendants {
                child_run_counts,
            } => {
                *child_run_counts.entry(child_run_id.clone()).or_default() += 1;
            }
            crate::flow_authority::FlowExecutionState::Terminal => {
                return Err(RuntimeError::ToolFailed(format!(
                    "flow identity '{parent_run_id}' is terminal"
                )));
            }
        }
        drop(state);
        Ok(DescendantBlockGuard {
            registry: Arc::clone(self),
            parent_run_id: parent_run_id.clone(),
            child_run_id: child_run_id.clone(),
        })
    }

    fn unblock_descendant(&self, parent_run_id: &FlowRunId, child_run_id: &FlowRunId) {
        self.with_lifecycle_arbitration(|| {
            self.unblock_descendant_locked(parent_run_id, child_run_id)
        });
    }

    fn unblock_descendant_locked(&self, parent_run_id: &FlowRunId, child_run_id: &FlowRunId) {
        let Some(parent) = self.lookup_run(parent_run_id) else {
            return;
        };
        let mut state = parent.execution_state.lock().unwrap();
        let crate::flow_authority::FlowExecutionState::BlockedOnDescendants { child_run_counts } =
            &mut *state
        else {
            return;
        };
        if let Some(count) = child_run_counts.get_mut(child_run_id) {
            *count -= 1;
            if *count == 0 {
                child_run_counts.remove(child_run_id);
            }
        }
        if child_run_counts.is_empty() {
            *state = crate::flow_authority::FlowExecutionState::Running;
        }
    }

    pub fn create_entry(
        &self,
        handle: String,
        goal: String,
        model: String,
        child_run_id: FlowRunId,
    ) -> Arc<FlowEntry> {
        self.create_entry_with_workspace(handle, goal, model, child_run_id, None)
    }

    pub fn create_entry_with_workspace(
        &self,
        handle: String,
        goal: String,
        model: String,
        child_run_id: FlowRunId,
        workspace: Option<WorkspaceBinding>,
    ) -> Arc<FlowEntry> {
        let display_label = goal.clone();
        self.create_entry_with_workspace_label(
            handle,
            goal,
            display_label,
            model,
            child_run_id,
            workspace,
        )
    }

    fn create_entry_with_workspace_label(
        &self,
        handle: String,
        goal: String,
        display_label: String,
        model: String,
        child_run_id: FlowRunId,
        workspace: Option<WorkspaceBinding>,
    ) -> Arc<FlowEntry> {
        let (stream_tx, _) = tokio::sync::broadcast::channel(64);
        let entry = Arc::new(FlowEntry {
            handle: handle.clone(),
            goal,
            display_label,
            status: Arc::new(Mutex::new(FlowRunStatus::Running {
                started_at: chrono::Utc::now(),
            })),
            output: Arc::new(Mutex::new(String::new())),
            cancel: tokio_util::sync::CancellationToken::new(),
            stream_tx,
            messages: Arc::new(Mutex::new(Vec::new())),
            iteration: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            child_run_id,
            model,
            started_at: chrono::Utc::now(),
            compact_lock: Arc::new(tokio::sync::Mutex::new(())),
            interjection_tx: tokio::sync::broadcast::channel(32).0,
            pending_injections: Arc::new(std::sync::Mutex::new(Vec::new())),
            injection_notify: Arc::new(tokio::sync::Notify::new()),
            frame_tx: tokio::sync::broadcast::channel(2048).0,
            workspace_state: Arc::new(Mutex::new(
                workspace.as_ref().map(|_| WorkspaceState::Active),
            )),
            workspace,
            cleanup_error: Arc::new(Mutex::new(None)),
        });
        self.entries
            .lock()
            .unwrap()
            .insert(handle, Arc::clone(&entry));
        if let Some(identity) = self.lookup_run(&entry.child_run_id) {
            self.bump_spawn_inventory(&identity.session_id);
        }
        entry
    }

    pub fn lookup(&self, handle: &str) -> Result<Arc<FlowEntry>, RuntimeError> {
        self.entries
            .lock()
            .unwrap()
            .get(handle)
            .map(Arc::clone)
            .ok_or_else(|| RuntimeError::ToolFailed(format!("agent: handle '{handle}' not found")))
    }

    pub fn remove(&self, handle: &str) {
        let removed = self.entries.lock().unwrap().remove(handle);
        if let Some(entry) = removed
            && let Some(identity) = self.lookup_run(&entry.child_run_id)
        {
            self.bump_spawn_inventory(&identity.session_id);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }
}

impl Tool for AgentSpawn {
    fn name(&self) -> &str {
        "flow.spawn"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Spawn a DSL flow as an independent sub-agent with its own message history and \
             iteration counter. Put target flow parameters in `arguments`. Use flow.search for \
             bounded discovery and flow.describe for the selected parameter contract.\n\n\
             Flow reference syntax: `file@flow_name`\n\
             - \"subagent.at@subagent\" — run the `subagent` flow in subagent.at\n\
             - \"subagent@research_loop\" — .at suffix optional\n\
             - \"subagent.at\" — no @, takes first non-describe flow\n\
             - \"/abs/path/my.at@main\" — absolute path\n\n\
             Default flow is `subagent.at` (research/verify/implement/review roles). \
             Required: `flow` and a single-use `spawn_token` from flow.instances. `version` rejects source changes after discovery. `async` is optional (default true). \
             Target flow parameters must be passed through `arguments`. \
             A flow may declare `contract { invocation { user_message: param } }` to seed its child message context. \
             Use flow.status/flow.output/flow.kill to manage async sub-agents by handle. \
             Pass only parameters declared by flow.describe. Missing params use flow-defined defaults.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "flow": {"type": "string", "description": "Flow reference (e.g. \"subagent.at@subagent\")."},
                "spawn_token": {"type": "string", "minLength": 1, "description": "Single-use admission token returned by the latest flow.instances call. Inspect and clean up existing flow instances before spawning."},
                "version": {"type": "string", "minLength": 1, "description": "Staleness guard. Pass the non-empty source fingerprint returned by the current flow.search or flow.describe result verbatim. Omit this field when no fingerprint is available; never send an empty or invented value."},
                "arguments": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Target flow parameters as key-value pairs. Use flow.search then flow.describe when the parameter contract is unknown. Example: arguments={\"goal\":\"read Cargo.toml\",\"role\":\"research\"}"
                },
                "async": {"type": "boolean", "default": true, "description": "If true (default), run in background and return a handle. If false, block until done."},
                "inherit_context": {"type": "boolean", "default": false, "description": "If true, seed the sub-agent's context with the parent snapshot through its last complete tool transaction. Every child also receives an explicit handoff.parent context record."},
                "workspace": {"type": "string", "enum": ["none", "auto", "retain"], "default": "none", "description": "Workspace policy for the child flow."}
            },
            "required": ["flow", "spawn_token"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let token = extract_spawn_token(&args)?;
            let flow_registry = ctx.flow_registry.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("flow.spawn: trusted flow registry is unavailable".into())
            })?;
            let identity = ctx.flow_identity.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "flow.spawn: trusted caller flow identity is unavailable".into(),
                )
            })?;
            flow_registry.consume_spawn_permit(&token, identity)?;
            let is_async = args
                .named("async")
                .and_then(|v| {
                    if let Value::Bool(b) = v {
                        Some(*b)
                    } else {
                        None
                    }
                })
                .unwrap_or(true);
            if is_async {
                run_sub_agent_async(args, ctx).await
            } else {
                run_sub_agent(args, ctx).await
            }
        })
    }
}

fn workspace_policy(args: &ToolArgs) -> Result<WorkspacePolicy, RuntimeError> {
    match args.named("workspace") {
        None => Ok(WorkspacePolicy::None),
        Some(Value::Str(value)) => value.parse().map_err(|error| {
            RuntimeError::ToolFailed(format!("flow.spawn: invalid workspace policy: {error}"))
        }),
        Some(_) => Err(RuntimeError::ToolFailed(
            "flow.spawn: workspace must be one of none, auto, or retain".into(),
        )),
    }
}

fn workspace_session(
    ctx: &ToolCtx,
    policy: WorkspacePolicy,
) -> Result<Option<String>, RuntimeError> {
    if policy == WorkspacePolicy::None {
        return Ok(ctx.session_id.clone().filter(|id| !id.is_empty()));
    }
    ctx.session_id
        .clone()
        .filter(|id| !id.is_empty())
        .map(Some)
        .ok_or_else(|| {
            RuntimeError::ToolFailed(
                "flow.spawn: workspace auto/retain requires a non-empty session id".into(),
            )
        })
}

fn allocate_workspace(
    ctx: &ToolCtx,
    policy: WorkspacePolicy,
    session_id: Option<&str>,
    run_id: &FlowRunId,
) -> Result<Option<WorkspaceBinding>, RuntimeError> {
    if policy == WorkspacePolicy::None {
        return Ok(None);
    }
    let service = ctx.flow_workspace_service.as_ref().ok_or_else(|| {
        RuntimeError::ToolFailed("flow.spawn: workspace service is unavailable".into())
    })?;
    service
        .allocate(
            policy,
            session_id.expect("validated managed workspace session"),
            &run_id.0.to_string(),
            ctx.workspace.as_ref().map(|binding| binding.path.as_path()),
        )
        .map_err(|error| RuntimeError::ToolFailed(format!("flow.spawn: {error}")))
}

struct WorkspaceFinalizeGuard {
    ctx: ToolCtx,
    binding: Option<WorkspaceBinding>,
    session_id: Option<String>,
    run_id: FlowRunId,
    state_projection: Option<Arc<Mutex<Option<WorkspaceState>>>>,
    error_projection: Option<Arc<Mutex<Option<String>>>>,
}

impl WorkspaceFinalizeGuard {
    fn new(
        ctx: &ToolCtx,
        binding: Option<WorkspaceBinding>,
        session_id: Option<String>,
        run_id: FlowRunId,
    ) -> Self {
        Self {
            ctx: ctx.clone(),
            binding,
            session_id,
            run_id,
            state_projection: None,
            error_projection: None,
        }
    }

    fn with_projections(
        mut self,
        state_projection: Arc<Mutex<Option<WorkspaceState>>>,
        error_projection: Arc<Mutex<Option<String>>>,
    ) -> Self {
        self.state_projection = Some(state_projection);
        self.error_projection = Some(error_projection);
        self
    }

    fn binding(&self) -> Option<&WorkspaceBinding> {
        self.binding.as_ref()
    }
}

impl Drop for WorkspaceFinalizeGuard {
    fn drop(&mut self) {
        finalize_workspace(
            &self.ctx,
            self.binding.as_ref(),
            self.session_id.as_deref(),
            &self.run_id,
            self.state_projection.as_ref(),
            self.error_projection.as_ref(),
        );
    }
}

fn finalize_workspace(
    ctx: &ToolCtx,
    binding: Option<&WorkspaceBinding>,
    session_id: Option<&str>,
    run_id: &FlowRunId,
    state_projection: Option<&Arc<Mutex<Option<WorkspaceState>>>>,
    error_projection: Option<&Arc<Mutex<Option<String>>>>,
) {
    let Some(binding) = binding else {
        return;
    };
    let service = ctx.flow_workspace_service.as_ref();
    let result = service
        .ok_or_else(|| "workspace service is unavailable".to_string())
        .and_then(|service| {
            service
                .finalize(
                    binding,
                    session_id.unwrap_or_default(),
                    &run_id.0.to_string(),
                )
                .map_err(|error| error.to_string())
        });
    let (state, cleanup_error) = match result {
        Ok(outcome) => {
            let record = match outcome {
                WorkspaceFinalizeOutcome::Released(record)
                | WorkspaceFinalizeOutcome::Dirty(record)
                | WorkspaceFinalizeOutcome::Retained(record)
                | WorkspaceFinalizeOutcome::AlreadyReleased(record) => record,
            };
            (record.lifecycle_state(), None)
        }
        Err(error) => (
            service
                .and_then(|service| service.persisted_state(binding))
                .unwrap_or_else(|| WorkspaceState::Unknown("unknown".into())),
            Some(error),
        ),
    };
    if let Some(projection) = state_projection {
        *projection.lock().unwrap() = Some(state.clone());
    }
    if let Some(projection) = error_projection {
        *projection.lock().unwrap() = cleanup_error.clone();
    }
    if let Some(sink) = &ctx.events {
        sink.emit(Event::WorkspaceLifecycle {
            run_id: run_id.clone(),
            workspace_id: binding.workspace_id.clone(),
            path: binding.path.display().to_string(),
            state: state.as_str().into(),
            cleanup_error,
        });
    }
}

struct PreparedFlowAgent {
    path: PathBuf,
    flow: atman_dsl::ast::FlowDecl,
    flows: std::collections::HashMap<String, atman_dsl::ast::FlowDecl>,
}

async fn prepare_flow_agent(
    flow_ref: &str,
    expected_version: Option<&str>,
) -> Result<PreparedFlowAgent, RuntimeError> {
    let (file_part, flow_name) = match flow_ref.split_once('@') {
        Some((file, name)) => (file, Some(name)),
        None => (flow_ref, None),
    };
    let (path, source) = read_flow_source(file_part).await?;
    let actual_version = format!("blake3:{}", blake3::hash(source.as_bytes()).to_hex());
    if expected_version.is_some_and(|version| version != actual_version) {
        return Err(RuntimeError::ToolFailed(format!(
            "flow.spawn: stale version for `{flow_ref}`; search again"
        )));
    }
    let file = atman_dsl::parse::parse_file(&source).map_err(|error| {
        RuntimeError::ToolFailed(format!("flow.spawn: parse {}: {error}", path.display()))
    })?;
    let flow = match flow_name {
        Some(name) => file.flows.iter().find(|flow| flow.name.name == name),
        None => file.flows.iter().find(|flow| flow.name.name != "describe"),
    }
    .cloned()
    .ok_or_else(|| {
        RuntimeError::ToolFailed(format!(
            "flow.spawn: target flow not found in {}",
            path.display()
        ))
    })?;
    let flows = file
        .flows
        .into_iter()
        .map(|flow| (flow.name.name.clone(), flow))
        .collect();
    Ok(PreparedFlowAgent { path, flow, flows })
}

fn register_prepared_identity(
    prepared: &PreparedFlowAgent,
    ctx: &ToolCtx,
    child_run_id: &FlowRunId,
    invocation: crate::flow_authority::InvocationKind,
    workspace: crate::flow_authority::ChildWorkspaceAuthority,
) -> Result<Arc<crate::flow_authority::FlowIdentity>, RuntimeError> {
    let registry = ctx.flow_registry.as_ref().ok_or_else(|| {
        RuntimeError::ToolFailed("flow.spawn: trusted flow registry is unavailable".into())
    })?;
    let parent = ctx.flow_identity.as_ref().ok_or_else(|| {
        RuntimeError::ToolFailed("flow.spawn: trusted parent flow identity is unavailable".into())
    })?;
    registry.register_child(
        &parent.run_id,
        child_run_id.clone(),
        invocation,
        crate::flow_authority::contract_allows_shell(prepared.flow.contract.as_ref()),
        workspace,
    )
}

fn spawned_workspace_authority(
    binding: Option<&WorkspaceBinding>,
) -> crate::flow_authority::ChildWorkspaceAuthority {
    match binding {
        Some(binding) => {
            crate::flow_authority::ChildWorkspaceAuthority::TrustedDelegation(binding.path.clone())
        }
        None => crate::flow_authority::ChildWorkspaceAuthority::Inherit,
    }
}

async fn run_sub_agent(args: ToolArgs, ctx: &ToolCtx) -> ToolResult {
    let flow = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".to_string());
    let version = extract_flow_version(&args)?;
    let prepared = prepare_flow_agent(&flow, version.as_deref()).await?;
    let flow_args = resolve_flow_arguments(&prepared.flow, &args)?;
    let inherit_context = should_inherit_context(&args);
    let run_id = FlowRunId::now();
    let policy = workspace_policy(&args)?;
    let session_id = workspace_session(ctx, policy)?;
    let workspace_guard = WorkspaceFinalizeGuard::new(
        ctx,
        allocate_workspace(ctx, policy, session_id.as_deref(), &run_id)?,
        session_id,
        run_id.clone(),
    );
    let child_identity = register_prepared_identity(
        &prepared,
        ctx,
        &run_id,
        crate::flow_authority::InvocationKind::SpawnSync,
        spawned_workspace_authority(workspace_guard.binding()),
    )?;
    let flow_registry = ctx.flow_registry.as_ref().expect("validated flow registry");
    let _lifecycle_guard = flow_registry.lifecycle_guard(&run_id);
    let parent_run_id = ctx
        .flow_identity
        .as_ref()
        .expect("validated parent flow identity")
        .run_id
        .clone();
    let _block_guard = flow_registry.block_on_descendant(&parent_run_id, &run_id)?;
    let mut child_ctx = match workspace_guard.binding().cloned() {
        Some(binding) => ctx.clone().with_workspace(binding),
        None => ctx.clone(),
    };
    child_ctx.flow_run_id = Some(run_id.clone());
    child_ctx.flow_identity = Some(child_identity);
    child_ctx.call_intent = None;
    let child_messages = Arc::new(Mutex::new(Vec::new()));
    if inherit_context && let Some(parent) = &ctx.session_messages_handle {
        *child_messages.lock().unwrap() = inherited_context_snapshot(parent);
    }
    run_prepared_flow_agent(
        prepared,
        flow_args,
        &child_ctx,
        run_id,
        child_messages,
        Arc::new(tokio::sync::Mutex::new(())),
        inherit_context,
    )
    .await
}

async fn run_sub_agent_async(args: ToolArgs, ctx: &ToolCtx) -> ToolResult {
    // Use the first string-typed argument as a display label for the FlowEntry.
    let goal = match args.named("arguments") {
        Some(Value::Struct(fields)) => fields.iter().find_map(|(_, value)| match value {
            Value::Str(value) => Some(value.clone()),
            _ => None,
        }),
        _ => None,
    }
    .unwrap_or_default();
    let display_label = ctx
        .call_intent
        .as_ref()
        .map(|intent| intent.as_str().to_string())
        .unwrap_or_else(|| goal.clone());
    let inherit_context = should_inherit_context(&args);
    let flow_registry = ctx.flow_registry.clone().ok_or_else(|| {
        RuntimeError::ToolFailed("flow.spawn: no agent registry available on ctx".into())
    })?;

    let flow_ref = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".to_string());
    let version = extract_flow_version(&args)?;
    let prepared = prepare_flow_agent(&flow_ref, version.as_deref()).await?;
    let flow_args = resolve_flow_arguments(&prepared.flow, &args)?;
    let model = flow_args
        .iter()
        .find_map(|(name, value)| match (name.as_str(), value) {
            ("model", Value::Str(model)) => Some(model.clone()),
            _ => None,
        })
        .or_else(|| {
            prepared
                .flow
                .params
                .iter()
                .find(|parameter| parameter.name.name == "model")
                .and_then(|parameter| match parameter.default.as_ref() {
                    Some(atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Str(model))) => {
                        Some(model.clone())
                    }
                    _ => None,
                })
        })
        .unwrap_or_default();

    let handle = format!("agent_{}", uuid::Uuid::now_v7().simple());
    let child_run_id = FlowRunId::now();
    let policy = workspace_policy(&args)?;
    let session_id = workspace_session(ctx, policy)?;
    let mut workspace_guard = WorkspaceFinalizeGuard::new(
        ctx,
        allocate_workspace(ctx, policy, session_id.as_deref(), &child_run_id)?,
        session_id.clone(),
        child_run_id.clone(),
    );
    let child_identity = register_prepared_identity(
        &prepared,
        ctx,
        &child_run_id,
        crate::flow_authority::InvocationKind::SpawnAsync,
        spawned_workspace_authority(workspace_guard.binding()),
    )?;
    let lifecycle_guard = flow_registry.lifecycle_guard(&child_run_id);
    let workspace = workspace_guard.binding().cloned();
    let entry = flow_registry.create_entry_with_workspace_label(
        handle.clone(),
        goal,
        display_label,
        model,
        child_run_id.clone(),
        workspace.clone(),
    );
    workspace_guard = workspace_guard.with_projections(
        Arc::clone(&entry.workspace_state),
        Arc::clone(&entry.cleanup_error),
    );
    if inherit_context {
        if let Some(parent) = &ctx.session_messages_handle {
            let snapshot = inherited_context_snapshot(parent);
            *entry.messages.lock().unwrap() = snapshot;
        }
    }

    let task_registry = ctx.task_registry.clone();
    let task_id = task_registry.as_ref().map(|tr| {
        tr.register_flow_with_run_id(
            entry.display_label.clone(),
            handle.clone(),
            session_id.clone().unwrap_or_default(),
            entry.cancel.clone(),
            workspace
                .as_ref()
                .map(|binding| binding.workspace_id.clone()),
            child_run_id.clone(),
        )
    });

    let entry_clone = Arc::clone(&entry);
    let ctx_clone = ctx.clone();
    let parent_stream_tx = ctx.stream_tx.clone();
    let child_run_id_str = child_run_id.0.to_string();
    let tool_use_id = ctx.tool_use_id.clone();
    tokio::spawn(async move {
        let _lifecycle_guard = lifecycle_guard;
        let _workspace_guard = workspace_guard;
        // Send SubAgentStarted so the TUI creates a SubAgentActivity item and
        // registers child_run_id in sub_agent_run_ids for frame routing.
        if let Some(tx) = &parent_stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::SubAgentStarted {
                handle: entry_clone.handle.clone(),
                tool_use_id,
                goal: entry_clone.display_label.clone(),
                child_run_id: child_run_id_str.clone(),
                model: entry_clone.model.clone(),
            });
        }

        // Replace ctx.cancel with entry.cancel so flow.kill can actually cancel
        // the sub-agent's flow execution. Pass entry into ctx so the DSL runtime
        // can write output/messages/iteration synchronously during LLM calls.
        // Bind the sub-agent's own compact_lock so it can compact its segment
        // independently.
        let mut ctx_for_flow = ctx_clone;
        ctx_for_flow.cancel = entry_clone.cancel.clone();
        ctx_for_flow.call_intent = None;
        ctx_for_flow.agent_entry = Some(Arc::clone(&entry_clone));
        ctx_for_flow.flow_run_id = Some(child_run_id.clone());
        ctx_for_flow.flow_identity = Some(child_identity);
        ctx_for_flow.compact_lock_handle = Some(Arc::clone(&entry_clone.compact_lock));
        if let Some(binding) = entry_clone.workspace.clone() {
            ctx_for_flow = ctx_for_flow.with_workspace(binding);
        }

        let result = run_prepared_flow_agent(
            prepared,
            flow_args,
            &ctx_for_flow,
            child_run_id.clone(),
            Arc::clone(&entry_clone.messages),
            Arc::clone(&entry_clone.compact_lock),
            inherit_context,
        )
        .await;
        let killed = entry_clone.cancel.is_cancelled();
        let status = match &result {
            _ if killed => FlowRunStatus::Killed {
                ended_at: chrono::Utc::now(),
            },
            Ok(Value::Str(s)) => FlowRunStatus::Ok {
                ended_at: chrono::Utc::now(),
                final_text: s.clone(),
            },
            Ok(_) => FlowRunStatus::Ok {
                ended_at: chrono::Utc::now(),
                final_text: String::new(),
            },
            Err(e) => FlowRunStatus::Err {
                ended_at: chrono::Utc::now(),
                message: e.to_string(),
            },
        };

        *entry_clone.status.lock().unwrap() = status.clone();

        // Send SubAgentDone so the TUI updates the SubAgentActivity item.
        if let Some(tx) = &parent_stream_tx {
            let final_text = match &status {
                FlowRunStatus::Ok { final_text, .. } => final_text.clone(),
                _ => String::new(),
            };
            let _ = tx.send(crate::stream::StreamFrame::SubAgentDone {
                handle: entry_clone.handle.clone(),
                status: status.kind_str().to_string(),
                final_text,
            });
        }

        let _ = entry_clone.stream_tx.send(FlowEvent::Exited { status });
        if let (Some(tr), Some(tid)) = (&task_registry, &task_id) {
            let ts = if killed {
                crate::task_registry::TaskStatus::Killed
            } else {
                match result {
                    Ok(_) => crate::task_registry::TaskStatus::Ok,
                    Err(_) => crate::task_registry::TaskStatus::Err,
                }
            };
            tr.finish(tid, ts);
        }
    });

    let mut fields = vec![
        ("handle".into(), Value::Str(handle)),
        ("status".into(), Value::Str("running".into())),
    ];
    if let Some(binding) = workspace {
        fields.push(("workspace_id".into(), Value::Str(binding.workspace_id)));
        fields.push((
            "workspace_path".into(),
            Value::Str(binding.path.display().to_string()),
        ));
        fields.push((
            "workspace_state".into(),
            Value::Str(WorkspaceState::Active.as_str().into()),
        ));
    }
    Ok(Value::Struct(fields))
}

pub struct AgentStatus;
impl Tool for AgentStatus {
    fn name(&self) -> &str {
        "flow.status"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Check the status of an async sub-agent. Returns handle, status, goal, and timing.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"handle": {"type": "string"}},
            "required": ["handle"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let reg = ctx
                .flow_registry
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.status: no agent registry".into()))?;
            let entry = reg.lookup(&handle)?;
            let st = entry.status.lock().unwrap().clone();
            let goal = entry.goal.clone();
            let mut fields = vec![
                ("handle".into(), Value::Str(handle)),
                ("status".into(), Value::Str(st.kind_str().into())),
                ("goal".into(), Value::Str(goal)),
            ];
            if let FlowRunStatus::Err { message, .. } = &st {
                fields.push(("error".into(), Value::Str(message.clone())));
            }
            if let Some(binding) = &entry.workspace {
                fields.push((
                    "workspace_id".into(),
                    Value::Str(binding.workspace_id.clone()),
                ));
                fields.push((
                    "workspace_path".into(),
                    Value::Str(binding.path.display().to_string()),
                ));
                if let Some(state) = entry.workspace_state.lock().unwrap().clone() {
                    fields.push(("workspace_state".into(), Value::Str(state.as_str().into())));
                }
                if let Some(error) = entry.cleanup_error.lock().unwrap().clone() {
                    fields.push(("cleanup_error".into(), Value::Str(error)));
                }
            }
            Ok(Value::Struct(fields))
        })
    }
}

pub struct AgentOutput;
impl Tool for AgentOutput {
    fn name(&self) -> &str {
        "flow.output"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Read accumulated assistant text from an async sub-agent.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "cursor": {"type": "integer", "default": 0},
                "limit": {"type": "integer", "default": 4096}
            },
            "required": ["handle"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let cursor = args
                .named("cursor")
                .and_then(|v| {
                    if let Value::Int(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
                .max(0) as usize;
            let limit = args
                .named("limit")
                .and_then(|v| {
                    if let Value::Int(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
                .unwrap_or(4096)
                .max(1) as usize;
            let reg = ctx
                .flow_registry
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.output: no agent registry".into()))?;
            let entry = reg.lookup(&handle)?;
            let output = entry.output.lock().unwrap().clone();
            let chunk = output.chars().skip(cursor).take(limit).collect::<String>();
            let next_cursor = cursor + chunk.chars().count();
            let eof = next_cursor >= output.chars().count();
            Ok(Value::Struct(vec![
                ("handle".into(), Value::Str(handle)),
                ("chunk".into(), Value::Str(chunk)),
                ("cursor".into(), Value::Int(cursor as i64)),
                ("next_cursor".into(), Value::Int(next_cursor as i64)),
                ("eof".into(), Value::Bool(eof)),
            ]))
        })
    }
}

pub struct AgentKill;
impl Tool for AgentKill {
    fn name(&self) -> &str {
        "flow.kill"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some("Cancel a running async sub-agent by handle.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"handle": {"type": "string"}},
            "required": ["handle"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let reg = ctx
                .flow_registry
                .clone()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.kill: no agent registry".into()))?;
            let entry = reg.lookup(&handle)?;
            entry.cancel.cancel();
            Ok(Value::Unit)
        })
    }
}

pub struct FlowInterject;
impl Tool for FlowInterject {
    fn name(&self) -> &str {
        "flow.interject"
    }
    fn tier(&self) -> Tier {
        Tier::Two
    }
    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Interject a text message into a running FlowRun (root or sub-agent) by handle. \
             L1 (nudge): inject text into context, flow continues. \
             L2 (course_correct): inject text, cancel current LLM call, flow continues with correction. \
             L3 (redirect): cancel current LLM call, redirect to a different flow. \
             L4 (hard_stop): kill the FlowRun immediately. \
             Use handle \"root\" to interject into the main agent.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "Target FlowRun handle (e.g. from flow.spawn return, or \"root\")."},
                "text": {"type": "string", "description": "Interjection text."},
                "level": {"type": "string", "enum": ["l1_nudge", "l2_course_correct", "l3_redirect", "l4_hard_stop"], "default": "l1_nudge", "description": "Interjection level."},
                "redirect_target": {"type": "string", "description": "Required for L3 redirect: the flow to redirect to."}
            },
            "required": ["handle", "text"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let text = extract_string(&args, "text", 1)?;
            let level_str = args
                .named("level")
                .and_then(|v| {
                    if let Value::Str(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "l1_nudge".to_string());
            let level = match level_str.as_str() {
                "l2_course_correct" => crate::injection::InjectionLevel::L2CourseCorrect,
                "l3_redirect" => crate::injection::InjectionLevel::L3Redirect,
                "l4_hard_stop" => crate::injection::InjectionLevel::L4HardStop,
                _ => crate::injection::InjectionLevel::L1Nudge,
            };
            let redirect_target = args.named("redirect_target").and_then(|v| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            });
            let reg = ctx.flow_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("flow.interject: no agent registry".into())
            })?;
            let entry = reg.lookup(&handle)?;
            let inj = crate::injection::Injection::with_level(
                crate::event::TurnId::now(),
                text,
                level,
                redirect_target,
            );
            entry.pending_injections.lock().unwrap().push(inj);
            entry.injection_notify.notify_one();
            Ok(Value::Unit)
        })
    }
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

async fn run_prepared_flow_agent(
    prepared: PreparedFlowAgent,
    flow_args: Vec<(String, Value)>,
    ctx: &ToolCtx,
    run_id: FlowRunId,
    child_messages: Arc<Mutex<Vec<Message>>>,
    child_compact_lock: Arc<tokio::sync::Mutex<()>>,
    inherited_parent_context: bool,
) -> ToolResult {
    let Some(registry) = ctx.registry.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "flow.spawn: no tool registry available on ctx".into(),
        ));
    };
    let Some(providers) = ctx.providers.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "flow.spawn: no provider registry available on ctx".into(),
        ));
    };
    let PreparedFlowAgent { path, flow, flows } = prepared;
    let initial_prompt = invocation_user_message(&flow, &flow_args)?;
    emit_flow_agent_start(ctx, &run_id, &flow.name.name);
    let mut child_ctx = sanitize_child_ctx(ctx);
    child_ctx.session_messages_handle = Some(child_messages);
    child_ctx.compact_lock_handle = Some(child_compact_lock);
    child_ctx.context_epoch_handle = Some(Arc::new(std::sync::atomic::AtomicU64::new(0)));
    child_ctx.context_prefix_tracker = Some(Arc::new(std::sync::Mutex::new(
        crate::context_plan::ContextPrefixTracker::default(),
    )));
    seed_parent_handoff_context(
        &child_ctx,
        &flow,
        &run_id,
        initial_prompt.as_deref(),
        inherited_parent_context,
    )?;
    if let Some(prompt) = initial_prompt {
        seed_child_message_context(&child_ctx, prompt)?;
    }
    let out = crate::exec::exec_flow_with_siblings(
        &flow,
        flow_args,
        registry.as_ref(),
        &child_ctx,
        providers.as_ref(),
        &flows,
        child_ctx.events.as_ref(),
        child_ctx.turn_id.clone(),
        Some(run_id.clone()),
        None,
        child_ctx.cancel.clone(),
        None,
        path.parent().map(|p| p.to_path_buf()),
    )
    .await;
    let status = match &out {
        Ok(_) => FlowStatus::Ok,
        Err(e) => FlowStatus::Errored {
            message: e.to_string(),
        },
    };
    mark_terminal_and_emit_child_flow_end(ctx, &run_id, &status);
    out
}

fn resolve_flow_arguments(
    flow: &atman_dsl::ast::FlowDecl,
    args: &ToolArgs,
) -> Result<Vec<(String, Value)>, RuntimeError> {
    let mut flow_args = Vec::new();
    let mut unknown = Vec::new();
    // Extract the structured flow parameters first.
    match args.named("arguments") {
        Some(Value::Struct(fields)) => {
            for (key, value) in fields {
                if flow
                    .params
                    .iter()
                    .any(|parameter| parameter.name.name == *key)
                {
                    flow_args.push((key.clone(), value.clone()));
                } else {
                    unknown.push(key.clone());
                }
            }
        }
        Some(Value::Unit) | None => {}
        Some(other) => {
            return Err(RuntimeError::ToolFailed(format!(
                "flow.spawn: `arguments` must be a struct, got {}",
                other.kind_name()
            )));
        }
    }
    // Backward compat: top-level named args (pre-arguments schema).
    for (key, value) in &args.named {
        if key == "flow"
            || key == "version"
            || key == "spawn_token"
            || key == "async"
            || key == "inherit_context"
            || key == "arguments"
            || key == "workspace"
        {
            continue;
        }
        if !flow
            .params
            .iter()
            .any(|parameter| parameter.name.name == *key)
        {
            unknown.push(key.clone());
        } else if flow_args.iter().any(|(name, _)| name == key) {
            return Err(RuntimeError::ToolFailed(format!(
                "flow.spawn: argument `{key}` was provided twice"
            )));
        } else {
            flow_args.push((key.clone(), value.clone()));
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        unknown.dedup();
        return Err(RuntimeError::ToolFailed(format!(
            "flow.spawn: unknown argument(s) for `{}`: {}",
            flow.name.name,
            unknown.join(", ")
        )));
    }
    Ok(flow_args)
}

fn should_inherit_context(args: &ToolArgs) -> bool {
    matches!(args.named("inherit_context"), Some(Value::Bool(true)))
}

fn inherited_context_snapshot(parent: &Arc<Mutex<Vec<Message>>>) -> Vec<Message> {
    let mut snapshot = parent.lock().unwrap().clone();
    crate::message::retain_complete_tool_pairs(&mut snapshot);
    snapshot
}

fn invocation_user_message(
    flow: &atman_dsl::ast::FlowDecl,
    flow_args: &[(String, Value)],
) -> Result<Option<String>, RuntimeError> {
    let Some((_, parameter_value)) = flow.contract.as_ref().and_then(|contract| {
        contract
            .blocks
            .iter()
            .find(|block| block.name.name == "invocation")
            .and_then(|block| {
                block
                    .kwargs
                    .iter()
                    .find(|(name, _)| name.name == "user_message")
            })
    }) else {
        return Ok(None);
    };
    let atman_dsl::ast::Expr::Ident(parameter_name) = parameter_value else {
        return Err(RuntimeError::ToolFailed(
            "flow.spawn: invocation user_message must reference a string flow parameter".into(),
        ));
    };
    let parameter_name = parameter_name.name.as_str();
    let Some(parameter) = flow
        .params
        .iter()
        .find(|parameter| parameter.name.name == parameter_name)
    else {
        return Err(RuntimeError::ToolFailed(format!(
            "flow.spawn: invocation user_message references unknown parameter `{parameter_name}`"
        )));
    };
    if !matches!(
        &parameter.ty,
        atman_dsl::ast::TypeExpr::Named(name) if name.name == "string"
    ) {
        return Err(RuntimeError::ToolFailed(format!(
            "flow.spawn: invocation user_message parameter `{parameter_name}` must be a string"
        )));
    }
    match flow_args
        .iter()
        .find(|(name, _)| name == parameter_name)
        .map(|(_, value)| value)
    {
        Some(Value::Str(value)) => Ok(Some(value.clone())),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: "string invocation user_message".into(),
            actual: value.kind_name().into(),
        }),
        None => match parameter.default.as_ref() {
            Some(atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Str(value))) => {
                Ok(Some(value.clone()))
            }
            Some(_) => Err(RuntimeError::ToolFailed(format!(
                "flow.spawn: invocation user_message parameter `{parameter_name}` requires a literal string default"
            ))),
            None => Err(RuntimeError::ToolFailed(format!(
                "flow.spawn: invocation user_message parameter `{parameter_name}` was not provided"
            ))),
        },
    }
}

fn seed_child_message_context(ctx: &ToolCtx, prompt: String) -> Result<(), RuntimeError> {
    let turn_id = ctx
        .turn_id
        .clone()
        .unwrap_or_else(crate::event::TurnId::now);
    crate::tools::session::append_message_to_context(
        ctx,
        crate::message::Message::user_text(turn_id, prompt),
    )
}

fn seed_parent_handoff_context(
    ctx: &ToolCtx,
    flow: &atman_dsl::ast::FlowDecl,
    child_run_id: &FlowRunId,
    invocation_prompt: Option<&str>,
    inherited_parent_context: bool,
) -> Result<(), RuntimeError> {
    let parent_run_id = ctx
        .flow_identity
        .as_ref()
        .and_then(|identity| identity.parent_run_id.as_ref())
        .map(ToString::to_string);
    let expected_result = flow
        .ret
        .as_ref()
        .map(super::flow_list::render_type)
        .unwrap_or_else(|| "unit".into());
    let task_digest = invocation_prompt
        .map(|prompt| format!("blake3:{}", blake3::hash(prompt.as_bytes()).to_hex()));
    let handoff = serde_json::json!({
        "parent_run_id": parent_run_id,
        "child_run_id": child_run_id.to_string(),
        "flow": flow.name.name,
        "task_source": if invocation_prompt.is_some() { "following_user_message" } else { "flow_parameters" },
        "task_digest": task_digest,
        "parent_context": if inherited_parent_context {
            "materialized_window_through_last_complete_tool_transaction"
        } else {
            "not_inherited"
        },
        "capability_boundary": "Only tools exposed by the current child model request are callable.",
        "task_boundary": "Execute only this delegated flow invocation and return its declared result to the parent.",
        "expected_result": expected_result,
    });
    let spec = crate::context_plan::ContextRecordSpec::new(
        "handoff.parent",
        crate::context_plan::ContextRecordAuthority::Runtime,
        crate::context_plan::ContextRecordRetention::Latest,
        crate::context_plan::ContextRecordBody::text(handoff.to_string()),
    );
    let turn_id = ctx
        .turn_id
        .clone()
        .unwrap_or_else(crate::event::TurnId::now);
    let record = {
        let messages = ctx.session_messages_handle.as_ref().ok_or_else(|| {
            RuntimeError::ToolFailed("flow.spawn: child message context is unavailable".into())
        })?;
        let messages = messages.lock().unwrap();
        crate::context_plan::compile_context_records(&messages, [spec])
            .into_iter()
            .next()
            .ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "flow.spawn: child handoff record was not materialized".into(),
                )
            })?
    };
    crate::tools::session::append_message_to_context(
        ctx,
        crate::message::Message::context_record(turn_id, record),
    )
}

fn mark_terminal_and_emit_child_flow_end(ctx: &ToolCtx, run_id: &FlowRunId, status: &FlowStatus) {
    terminal_then_emit(
        || {
            if let Some(flow_registry) = &ctx.flow_registry {
                flow_registry.mark_terminal(run_id);
            }
        },
        || emit_child_flow_end(ctx, run_id, status),
    );
}

fn terminal_then_emit(mark_terminal: impl FnOnce(), emit: impl FnOnce()) {
    mark_terminal();
    emit();
}

fn extract_flow(args: &ToolArgs) -> Result<Option<String>, RuntimeError> {
    match args.named("flow") {
        Some(Value::Str(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "flow string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_flow_version(args: &ToolArgs) -> Result<Option<String>, RuntimeError> {
    match args.named("version") {
        Some(Value::Str(version)) if !version.trim().is_empty() => Ok(Some(version.clone())),
        Some(Value::Str(_)) => Ok(None),
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "version string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_spawn_token(args: &ToolArgs) -> Result<String, RuntimeError> {
    match args.named("spawn_token") {
        Some(Value::Str(token)) if !token.trim().is_empty() => Ok(token.clone()),
        _ => Err(RuntimeError::ToolFailed(
            "flow.spawn: spawn_token is required; call flow.instances, inspect existing flows, reuse suitable work, and kill obsolete flows before spawning"
                .into(),
        )),
    }
}

async fn read_flow_source(flow_ref: &str) -> Result<(PathBuf, String), RuntimeError> {
    for path in super::flow_source::candidates(flow_ref) {
        match tokio::fs::read_to_string(&path).await {
            Ok(src) => return Ok((path, src)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RuntimeError::ToolFailed(format!(
                    "flow.spawn: read {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(RuntimeError::ToolFailed(format!(
        "flow.spawn: flow `{flow_ref}` not found"
    )))
}

fn emit_flow_agent_start(ctx: &ToolCtx, run_id: &FlowRunId, flow_name: &str) {
    let parent_run_id = ctx
        .flow_identity
        .as_ref()
        .and_then(|identity| identity.parent_run_id.clone())
        .or_else(|| {
            ctx.flow_run_id
                .clone()
                .filter(|candidate| candidate != run_id)
        });
    let parent_node_id = ctx.current_node_id.clone();
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowStart {
            run_id: run_id.clone(),
            flow_name: flow_name.into(),
            parent_run_id: parent_run_id.clone(),
            parent_node_id: parent_node_id.clone(),
            spawned: true,
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowStart {
            run_id: run_id.0.to_string(),
            flow_name: flow_name.into(),
            parent_run_id: parent_run_id.as_ref().map(|r| r.0.to_string()),
            parent_node_id,
        });
    }
}

fn emit_child_flow_end(ctx: &ToolCtx, run_id: &FlowRunId, status: &FlowStatus) {
    let suicide = ctx.task_registry.as_ref().is_some_and(|registry| {
        registry
            .list(&crate::task_registry::TaskFilter::default())
            .iter()
            .any(|snapshot| {
                snapshot.flow_run_id.as_ref() == Some(run_id)
                    && snapshot.termination == Some(crate::task_registry::TaskTermination::Suicide)
            })
    });
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowEnd {
            run_id: run_id.clone(),
            flow_name: "agent.sub".into(),
            status: status.clone(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowDone {
            run_id: run_id.0.to_string(),
            flow_name: "agent.sub".into(),
            ok: matches!(status, FlowStatus::Ok),
            cancelled: matches!(status, FlowStatus::Cancelled),
            suicide,
        });
    }
}

fn sanitize_child_ctx(parent: &ToolCtx) -> ToolCtx {
    let mut c = parent.clone();
    c.session_runtime = None;
    c.history_segment = crate::tool::HistorySegment::Spawned;
    c.session_messages_handle = None;
    c.compact_lock_handle = None;
    c.context_epoch_handle = None;
    c.context_prefix_tracker = None;
    c.forms = None;
    c.on_memory_recent = None;
    c
}

#[cfg(test)]
mod tests {
    use super::{
        AgentSpawn, FlowRegistry, FlowRunStatus, extract_flow_version, inherited_context_snapshot,
        prepare_flow_agent, resolve_flow_arguments, terminal_then_emit,
    };
    use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
    use crate::permission::PermissionBroker;
    use crate::provider::ProviderRegistry;
    use crate::tool::{Tier, Tool, ToolArgs, ToolCtx, ToolRegistry};
    use crate::{InvocationEnv, Value};
    use std::cell::Cell;
    use std::sync::Arc;

    struct SandboxProbe;

    struct SessionTextProbe;

    fn root_identity(
        registry: &FlowRegistry,
        session_id: &str,
    ) -> Arc<crate::flow_authority::FlowIdentity> {
        let run_id = crate::event::FlowRunId::now();
        registry
            .register_root(
                session_id.to_owned(),
                run_id,
                crate::flow_authority::EffectiveAuthority::root(&Default::default(), false, None),
            )
            .unwrap()
    }

    fn admitted_args(registry: &FlowRegistry, ctx: &ToolCtx, mut args: ToolArgs) -> ToolArgs {
        args.named.push((
            "spawn_token".into(),
            Value::Str(
                registry.issue_spawn_permit(ctx.flow_identity.as_ref().expect("flow identity")),
            ),
        ));
        args
    }

    #[test]
    fn spawn_permits_are_single_use_and_bound_to_the_caller() {
        let registry = FlowRegistry::new();
        let owner = root_identity(&registry, "session");
        let sibling = root_identity(&registry, "session");
        let foreign = root_identity(&registry, "other-session");

        let token = registry.issue_spawn_permit(&owner);
        assert!(registry.consume_spawn_permit(&token, &foreign).is_err());

        let token = registry.issue_spawn_permit(&owner);
        assert!(registry.consume_spawn_permit(&token, &sibling).is_err());

        let token = registry.issue_spawn_permit(&owner);
        registry.consume_spawn_permit(&token, &owner).unwrap();
        assert!(registry.consume_spawn_permit(&token, &owner).is_err());
    }

    #[test]
    fn spawn_inventory_changes_invalidate_issued_permits() {
        let registry = FlowRegistry::new();
        let owner = root_identity(&registry, "session");
        let token = registry.issue_spawn_permit(&owner);

        registry.bump_spawn_inventory("session");

        assert!(registry.consume_spawn_permit(&token, &owner).is_err());
    }

    #[test]
    fn empty_spawn_version_is_treated_as_omitted() {
        for value in ["", " ", "\t\n"] {
            let args = ToolArgs {
                named: vec![("version".into(), Value::Str(value.into()))],
                ..Default::default()
            };
            assert_eq!(extract_flow_version(&args).unwrap(), None);
        }
    }

    #[tokio::test]
    async fn prepared_flow_rejects_a_stale_discovery_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("child.at");
        let source = "flow child(goal: string) -> string { return goal }";
        std::fs::write(&path, source).unwrap();
        let flow_ref = format!("{}@child", path.display());
        let version = format!("blake3:{}", blake3::hash(source.as_bytes()).to_hex());

        prepare_flow_agent(&flow_ref, Some(&version)).await.unwrap();
        let error = match prepare_flow_agent(&flow_ref, Some("blake3:stale")).await {
            Ok(_) => panic!("stale version should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("stale version"));
    }

    #[test]
    fn flow_arguments_reject_unknown_and_duplicate_fields() {
        let file = atman_dsl::parse::parse_file(
            "flow child(goal: string, retries: int = 1) -> string { return goal }",
        )
        .unwrap();
        let flow = &file.flows[0];
        let unknown = ToolArgs {
            named: vec![(
                "arguments".into(),
                Value::Struct(vec![
                    ("goal".into(), Value::Str("work".into())),
                    ("typo".into(), Value::Bool(true)),
                ]),
            )],
            ..Default::default()
        };
        let error = resolve_flow_arguments(flow, &unknown).unwrap_err();
        assert!(error.to_string().contains("unknown argument(s)"));
        assert!(error.to_string().contains("typo"));

        let duplicate = ToolArgs {
            named: vec![
                (
                    "arguments".into(),
                    Value::Struct(vec![("goal".into(), Value::Str("nested".into()))]),
                ),
                ("goal".into(), Value::Str("top-level".into())),
            ],
            ..Default::default()
        };
        assert!(
            resolve_flow_arguments(flow, &duplicate)
                .unwrap_err()
                .to_string()
                .contains("provided twice")
        );
    }

    #[test]
    fn flow_entry_keeps_execution_goal_separate_from_display_label() {
        let registry = FlowRegistry::new();
        let entry = registry.create_entry_with_workspace_label(
            "agent-test".into(),
            "Audit the full provider chain".into(),
            "Review provider routing".into(),
            "smart".into(),
            crate::event::FlowRunId::now(),
            None,
        );
        assert_eq!(entry.goal, "Audit the full provider chain");
        assert_eq!(entry.display_label, "Review provider routing");
    }

    #[test]
    fn inherited_context_excludes_incomplete_parent_tool_transactions() {
        let turn_id = crate::event::TurnId::now();
        let parent = Arc::new(std::sync::Mutex::new(vec![
            Message::user_text(turn_id.clone(), "delegate work"),
            Message {
                role: MessageRole::Assistant,
                parts: vec![
                    MessagePart::Text {
                        text: "starting tools".into(),
                    },
                    MessagePart::ToolUse {
                        id: "active-spawn".into(),
                        name: "flow.spawn".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    },
                    MessagePart::ToolUse {
                        id: "complete-read".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({"path": "README.md"}),
                        intent: None,
                    },
                ],
                turn_id: turn_id.clone(),
                origin: MessageOrigin::User,
            },
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "complete-read".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
                turn_id,
                origin: MessageOrigin::User,
            },
        ]));

        let snapshot = inherited_context_snapshot(&parent);

        assert!(snapshot.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Text { text } if text == "starting tools"))
        }));
        assert!(snapshot.iter().any(|message| {
            message.parts.iter().any(
                |part| matches!(part, MessagePart::ToolUse { id, .. } if id == "complete-read"),
            )
        }));
        assert!(!snapshot.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ToolUse { id, .. } if id == "active-spawn"))
        }));
        assert!(parent.lock().unwrap().iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ToolUse { id, .. } if id == "active-spawn"))
        }));
    }

    impl Tool for SandboxProbe {
        fn name(&self) -> &str {
            "sandbox.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(
            &'a self,
            _args: ToolArgs,
            ctx: &'a ToolCtx,
        ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
            Box::pin(async move { Ok(Value::Bool(ctx.sandbox.is_some())) })
        }
    }

    impl Tool for SessionTextProbe {
        fn name(&self) -> &str {
            "session.text"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(
            &'a self,
            _args: ToolArgs,
            ctx: &'a ToolCtx,
        ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
            Box::pin(async move {
                let text = ctx
                    .session_messages_handle
                    .as_ref()
                    .map(|handle| {
                        handle
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|message| {
                                !message
                                    .parts
                                    .iter()
                                    .any(|part| matches!(part, MessagePart::ContextRecord(_)))
                            })
                            .map(crate::message::Message::text_concat)
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .unwrap_or_default();
                Ok(Value::Str(text))
            })
        }
    }

    #[test]
    fn terminal_transition_happens_before_flow_end_emit() {
        let terminal = Cell::new(false);

        terminal_then_emit(
            || terminal.set(true),
            || assert!(terminal.get(), "FlowEnd emitted before terminal transition"),
        );
    }

    #[tokio::test]
    async fn sync_and_async_spawned_flows_inherit_invocation_snapshot() {
        for is_async in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("child.at");
            std::fs::write(&path, r#"flow child() -> string { return env("effort") }"#).unwrap();

            let registry = Arc::new(FlowRegistry::new());
            let root_run_id = crate::event::FlowRunId::now();
            let root_identity = registry
                .register_root(
                    "test-session".into(),
                    root_run_id.clone(),
                    crate::flow_authority::EffectiveAuthority::root(
                        &Default::default(),
                        false,
                        None,
                    ),
                )
                .unwrap();
            let mut ctx = ToolCtx::new()
                .with_registry(Arc::new(ToolRegistry::new()))
                .with_providers(Arc::new(ProviderRegistry::new()))
                .with_flow_registry(Arc::clone(&registry))
                .with_invocation_env(InvocationEnv::single("effort", Value::Str("high".into())));
            ctx.flow_run_id = Some(root_run_id);
            ctx.flow_identity = Some(root_identity);

            let result = AgentSpawn
                .call(
                    admitted_args(
                        &registry,
                        &ctx,
                        ToolArgs {
                            positional: Vec::new(),
                            named: vec![
                                (
                                    "flow".into(),
                                    Value::Str(format!("{}@child", path.display())),
                                ),
                                ("async".into(), Value::Bool(is_async)),
                            ],
                        },
                    ),
                    &ctx,
                )
                .await
                .unwrap();

            if !is_async {
                assert!(matches!(result, Value::Str(value) if value == "high"));
                continue;
            }

            let Value::Struct(fields) = result else {
                panic!("expected async spawn handle");
            };
            let handle = fields
                .into_iter()
                .find_map(|(name, value)| {
                    (name == "handle")
                        .then_some(value)
                        .and_then(|value| match value {
                            Value::Str(handle) => Some(handle),
                            _ => None,
                        })
                })
                .expect("async spawn handle");
            let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let status = registry
                        .lookup(&handle)
                        .unwrap()
                        .status
                        .lock()
                        .unwrap()
                        .clone();
                    if !status.is_running() {
                        break status;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(matches!(
                status,
                FlowRunStatus::Ok { final_text, .. } if final_text == "high"
            ));
        }
    }

    #[tokio::test]
    async fn async_spawn_announces_declared_model() {
        for (arguments, expected) in [
            (Vec::new(), "smart"),
            (
                vec![("model".into(), Value::Str("vendor/model".into()))],
                "vendor/model",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("child.at");
            std::fs::write(
                &path,
                r#"flow child(model: string = "smart") -> string { return model }"#,
            )
            .unwrap();

            let registry = Arc::new(FlowRegistry::new());
            let root_run_id = crate::event::FlowRunId::now();
            let root_identity = registry
                .register_root(
                    "test-session".into(),
                    root_run_id.clone(),
                    crate::flow_authority::EffectiveAuthority::root(
                        &Default::default(),
                        false,
                        None,
                    ),
                )
                .unwrap();
            let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(16);
            let mut ctx = ToolCtx::new()
                .with_registry(Arc::new(ToolRegistry::new()))
                .with_providers(Arc::new(ProviderRegistry::new()))
                .with_flow_registry(Arc::clone(&registry))
                .with_stream_tx(stream_tx);
            ctx.flow_run_id = Some(root_run_id);
            ctx.flow_identity = Some(root_identity);

            AgentSpawn
                .call(
                    admitted_args(
                        &registry,
                        &ctx,
                        ToolArgs {
                            positional: Vec::new(),
                            named: vec![
                                (
                                    "flow".into(),
                                    Value::Str(format!("{}@child", path.display())),
                                ),
                                ("async".into(), Value::Bool(true)),
                                ("arguments".into(), Value::Struct(arguments)),
                            ],
                        },
                    ),
                    &ctx,
                )
                .await
                .unwrap();

            let announced_model = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let crate::stream::StreamFrame::SubAgentStarted { model, .. } =
                        stream_rx.recv().await.unwrap()
                    {
                        break model;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(announced_model, expected);
        }
    }

    #[tokio::test]
    async fn flow_spawn_retains_sandbox_for_child_tool_invocations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("child.at");
        std::fs::write(&path, r#"flow child() -> bool { return sandbox.probe() }"#).unwrap();

        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(SandboxProbe));
        let flows = Arc::new(FlowRegistry::new());
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let root_run_id = crate::event::FlowRunId::now();
        let trust = crate::trust::TrustConfig::default();
        let root_identity = flows
            .register_root(
                "test-session".into(),
                root_run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&trust, false, None),
            )
            .unwrap();
        let sandbox: Arc<dyn crate::sandbox::Sandbox> =
            Arc::new(crate::sandbox::SandboxExec::new(dir.path()));
        let mut ctx = ToolCtx::new()
            .with_registry(tools)
            .with_providers(Arc::new(ProviderRegistry::new()))
            .with_flow_registry(Arc::clone(&flows))
            .with_permission_broker(broker)
            .with_session_id("test-session")
            .with_trust(trust)
            .with_sandbox(sandbox)
            .for_tool_invocation(Tier::Two);
        ctx.flow_run_id = Some(root_run_id);
        ctx.flow_identity = Some(root_identity);

        let result = AgentSpawn
            .call(
                admitted_args(
                    &flows,
                    &ctx,
                    ToolArgs {
                        positional: Vec::new(),
                        named: vec![
                            (
                                "flow".into(),
                                Value::Str(format!("{}@child", path.display())),
                            ),
                            ("async".into(), Value::Bool(false)),
                        ],
                    },
                ),
                &ctx,
            )
            .await
            .unwrap();

        assert!(matches!(result, Value::Bool(true)));
    }

    #[tokio::test]
    async fn flow_spawn_owns_one_isolated_invocation_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("child.at");
        std::fs::write(
            &path,
            r#"flow child(user_prompt: string) -> string {
    contract { invocation { user_message: user_prompt } }
    return session.text()
}

flow plain(user_prompt: string) -> string {
    return session.text()
}"#,
        )
        .unwrap();

        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(SessionTextProbe));
        let flows = Arc::new(FlowRegistry::new());
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let event_session = Arc::new(crate::session::Session::open_ephemeral());
        let root_run_id = crate::event::FlowRunId::now();
        let trust = crate::trust::TrustConfig::default();
        let root_identity = flows
            .register_root(
                "test-session".into(),
                root_run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&trust, false, None),
            )
            .unwrap();
        let root_entry = flows.create_entry(
            "root".into(),
            "parent prompt".into(),
            String::new(),
            root_run_id.clone(),
        );
        let parent_messages = Arc::new(std::sync::Mutex::new(vec![
            crate::message::Message::user_text(crate::event::TurnId::now(), "parent prompt"),
        ]));
        let mut ctx = ToolCtx::new()
            .with_registry(tools)
            .with_providers(Arc::new(ProviderRegistry::new()))
            .with_flow_registry(Arc::clone(&flows))
            .with_permission_broker(broker)
            .with_events(event_session.sink().clone())
            .with_session_id("test-session")
            .with_trust(trust)
            .with_agent_entry(Arc::clone(&root_entry))
            .with_session_messages_handle(Arc::clone(&parent_messages));
        ctx.flow_run_id = Some(root_run_id);
        ctx.flow_identity = Some(root_identity);

        let result = AgentSpawn
            .call(
                admitted_args(
                    &flows,
                    &ctx,
                    ToolArgs {
                        positional: Vec::new(),
                        named: vec![
                            (
                                "flow".into(),
                                Value::Str(format!("{}@child", path.display())),
                            ),
                            ("async".into(), Value::Bool(false)),
                            (
                                "arguments".into(),
                                Value::Struct(vec![(
                                    "user_prompt".into(),
                                    Value::Str("child prompt".into()),
                                )]),
                            ),
                        ],
                    },
                ),
                &ctx,
            )
            .await
            .unwrap();

        assert!(matches!(result, Value::Str(text) if text == "child prompt"));
        assert_eq!(parent_messages.lock().unwrap().len(), 1);
        assert!(
            root_entry.messages.lock().unwrap().is_empty(),
            "sync child must not reuse the parent FlowEntry message segment"
        );

        let plain_result = AgentSpawn
            .call(
                admitted_args(
                    &flows,
                    &ctx,
                    ToolArgs {
                        positional: Vec::new(),
                        named: vec![
                            (
                                "flow".into(),
                                Value::Str(format!("{}@plain", path.display())),
                            ),
                            ("async".into(), Value::Bool(false)),
                            (
                                "arguments".into(),
                                Value::Struct(vec![(
                                    "user_prompt".into(),
                                    Value::Str("not implicit".into()),
                                )]),
                            ),
                        ],
                    },
                ),
                &ctx,
            )
            .await
            .unwrap();

        assert!(matches!(plain_result, Value::Str(text) if text.is_empty()));

        let async_result = AgentSpawn
            .call(
                admitted_args(
                    &flows,
                    &ctx,
                    ToolArgs {
                        positional: Vec::new(),
                        named: vec![
                            (
                                "flow".into(),
                                Value::Str(format!("{}@child", path.display())),
                            ),
                            ("async".into(), Value::Bool(true)),
                            (
                                "arguments".into(),
                                Value::Struct(vec![(
                                    "user_prompt".into(),
                                    Value::Str("async child prompt".into()),
                                )]),
                            ),
                        ],
                    },
                ),
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(fields) = async_result else {
            panic!("expected async spawn handle");
        };
        let handle = fields
            .into_iter()
            .find_map(|(name, value)| match (name.as_str(), value) {
                ("handle", Value::Str(handle)) => Some(handle),
                _ => None,
            })
            .expect("async spawn handle");
        let entry = flows.lookup(&handle).unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = entry.status.lock().unwrap().clone();
                if !status.is_running() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            status,
            FlowRunStatus::Ok { final_text, .. } if final_text == "async child prompt"
        ));
        let entry_messages = entry.messages.lock().unwrap();
        assert_eq!(entry_messages.len(), 2);
        assert!(matches!(
            entry_messages[0].parts.as_slice(),
            [MessagePart::ContextRecord(record)] if record.key() == "handoff.parent"
        ));
        let MessagePart::ContextRecord(handoff) = &entry_messages[0].parts[0] else {
            unreachable!("validated handoff record")
        };
        let rendered_handoff = handoff.render_for_model();
        assert!(rendered_handoff.contains("following_user_message"));
        assert!(rendered_handoff.contains("expected_result"));
        assert!(!rendered_handoff.contains("async child prompt"));
        assert_eq!(entry_messages[1].text_concat(), "async child prompt");
        drop(entry_messages);
        assert_eq!(parent_messages.lock().unwrap().len(), 1);
        assert!(root_entry.messages.lock().unwrap().is_empty());
        assert!(
            event_session.messages_full().is_empty(),
            "spawned handoff records must not project into root history"
        );
        let invocation_events = event_session
            .sink()
            .snapshot()
            .into_iter()
            .filter_map(|event| match event {
                crate::event::Event::UserMsg {
                    flow_run_id,
                    message,
                    ..
                } => Some((flow_run_id, message.text_concat())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(invocation_events.len(), 2);
        assert!(
            invocation_events
                .iter()
                .all(|(flow_run_id, _)| flow_run_id.is_some()),
            "child invocation events must not project into root history"
        );
        let handoff_events = event_session
            .sink()
            .snapshot()
            .into_iter()
            .filter_map(|event| match event {
                crate::event::Event::SystemMsg {
                    flow_run_id,
                    message,
                    ..
                } if message.parts.iter().any(
                    |part| matches!(part, MessagePart::ContextRecord(record) if record.key() == "handoff.parent"),
                ) => Some(flow_run_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(handoff_events.len(), 3);
        assert!(handoff_events.iter().all(Option::is_some));
    }
}
