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
        let (stream_tx, _) = tokio::sync::broadcast::channel(64);
        let entry = Arc::new(FlowEntry {
            handle: handle.clone(),
            goal,
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
            frame_tx: tokio::sync::broadcast::channel(256).0,
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
        self.entries.lock().unwrap().remove(handle);
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
             iteration counter. All named args except `flow` and `async` pass through to the \
             flow as parameters — call flow.list first to discover available flows and their \
             parameter signatures.\n\n\
             Flow reference syntax: `file@flow_name`\n\
             - \"subagent.at@subagent\" — run the `subagent` flow in subagent.at\n\
             - \"subagent@research_loop\" — .at suffix optional\n\
             - \"subagent.at\" — no @, takes first non-describe flow\n\
             - \"/abs/path/my.at@main\" — absolute path\n\n\
             Default flow is `subagent.at` (research/verify/implement/review roles). \
             Required: `flow`. `async` is optional (default true). Other named args pass through to the flow. \
             Use flow.status/flow.output/flow.kill to manage async sub-agents by handle. \
             Best practice: call flow.list to see available flows and params, then pass \
             matching named args. Missing params use flow-defined defaults.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "flow": {"type": "string", "description": "Flow reference (e.g. \"subagent.at@subagent\")."},
                "arguments": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Target flow parameters as key-value pairs. You MUST call flow.list first to discover the flow's description and parameter signatures (names, types, required/optional), then construct this object accordingly. Example: arguments={\"goal\":\"read Cargo.toml\",\"role\":\"research\"}"
                },
                "async": {"type": "boolean", "default": true, "description": "If true (default), run in background and return a handle. If false, block until done."},
                "inherit_context": {"type": "boolean", "default": false, "description": "If true, seed the sub-agent's context with a snapshot of the parent's messages."},
                "workspace": {"type": "string", "enum": ["none", "auto", "retain"], "default": "none", "description": "Workspace policy for the child flow."}
            },
            "required": ["flow"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
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

async fn prepare_flow_agent(flow_ref: &str) -> Result<PreparedFlowAgent, RuntimeError> {
    let (file_part, flow_name) = match flow_ref.split_once('@') {
        Some((file, name)) => (file, Some(name)),
        None => (flow_ref, None),
    };
    let (path, source) = read_flow_source(file_part).await?;
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
    let run_id = FlowRunId::now();
    let policy = workspace_policy(&args)?;
    let session_id = workspace_session(ctx, policy)?;
    let workspace_guard = WorkspaceFinalizeGuard::new(
        ctx,
        allocate_workspace(ctx, policy, session_id.as_deref(), &run_id)?,
        session_id,
        run_id.clone(),
    );
    let prepared = prepare_flow_agent(&flow).await?;
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
    run_prepared_flow_agent(prepared, &args, &child_ctx, run_id).await
}

async fn run_sub_agent_async(args: ToolArgs, ctx: &ToolCtx) -> ToolResult {
    // Extract flow params from the `arguments` object (generic, no hardcoded param names).
    let arg_fields: Vec<(String, Value)> = match args.named("arguments") {
        Some(Value::Struct(fields)) => fields.clone(),
        _ => Vec::new(),
    };
    // Use the first string-typed argument as a display label for the FlowEntry.
    let display_label = arg_fields
        .iter()
        .find_map(|(_, v)| {
            if let Value::Str(s) = v {
                Some(s.clone())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let inherit_context = args
        .named("inherit_context")
        .map(|v| matches!(v, Value::Bool(true)))
        .unwrap_or(false);
    let flow_registry = ctx.flow_registry.clone().ok_or_else(|| {
        RuntimeError::ToolFailed("flow.spawn: no agent registry available on ctx".into())
    })?;

    let flow_ref = extract_flow(&args)?.unwrap_or_else(|| "subagent.at".to_string());

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
    let prepared = prepare_flow_agent(&flow_ref).await?;
    let child_identity = register_prepared_identity(
        &prepared,
        ctx,
        &child_run_id,
        crate::flow_authority::InvocationKind::SpawnAsync,
        spawned_workspace_authority(workspace_guard.binding()),
    )?;
    let lifecycle_guard = flow_registry.lifecycle_guard(&child_run_id);
    let workspace = workspace_guard.binding().cloned();
    let entry = flow_registry.create_entry_with_workspace(
        handle.clone(),
        display_label,
        String::new(),
        child_run_id.clone(),
        workspace.clone(),
    );
    workspace_guard = workspace_guard.with_projections(
        Arc::clone(&entry.workspace_state),
        Arc::clone(&entry.cleanup_error),
    );
    if inherit_context {
        if let Some(parent) = &ctx.session_messages_handle {
            let snapshot = parent.lock().unwrap().clone();
            *entry.messages.lock().unwrap() = snapshot;
        }
    }

    let task_registry = ctx.task_registry.clone();
    let task_id = task_registry.as_ref().map(|tr| {
        tr.register_flow_with_run_id(
            entry.goal.clone(),
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
    tokio::spawn(async move {
        let _lifecycle_guard = lifecycle_guard;
        let _workspace_guard = workspace_guard;
        // Send SubAgentStarted so the TUI creates a SubAgentActivity item and
        // registers child_run_id in sub_agent_run_ids for frame routing.
        if let Some(tx) = &parent_stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::SubAgentStarted {
                handle: entry_clone.handle.clone(),
                goal: entry_clone.goal.clone(),
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
        ctx_for_flow.agent_entry = Some(Arc::clone(&entry_clone));
        ctx_for_flow.flow_run_id = Some(child_run_id.clone());
        ctx_for_flow.flow_identity = Some(child_identity);
        ctx_for_flow.compact_lock_handle = Some(Arc::clone(&entry_clone.compact_lock));
        if let Some(binding) = entry_clone.workspace.clone() {
            ctx_for_flow = ctx_for_flow.with_workspace(binding);
        }

        let result =
            run_prepared_flow_agent(prepared, &args, &ctx_for_flow, child_run_id.clone()).await;
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
    args: &ToolArgs,
    ctx: &ToolCtx,
    run_id: FlowRunId,
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
    // Extract flow params from the `arguments` object — generic, matches by
    // param name, no hardcoded param names.
    let mut flow_args: Vec<(String, Value)> = Vec::new();
    if let Some(Value::Struct(fields)) = args.named("arguments") {
        for (key, value) in fields {
            if key == "flow" || key == "async" || key == "inherit_context" || key == "workspace" {
                continue;
            }
            if flow.params.iter().any(|p| p.name.name == *key) {
                flow_args.push((key.clone(), value.clone()));
            }
        }
    }
    // Backward compat: top-level named args (pre-arguments schema).
    for (key, value) in &args.named {
        if key == "flow"
            || key == "async"
            || key == "inherit_context"
            || key == "arguments"
            || key == "workspace"
        {
            continue;
        }
        if flow.params.iter().any(|p| p.name.name == *key)
            && !flow_args.iter().any(|(k, _)| k == key)
        {
            flow_args.push((key.clone(), value.clone()));
        }
    }
    emit_flow_agent_start(ctx, &run_id, &flow.name.name);
    let mut child_ctx = sanitize_child_ctx(ctx);
    // One message segment per FlowRun: async shares entry.messages, sync gets
    // a fresh ephemeral handle.
    let inherit = args
        .named("inherit_context")
        .map(|v| matches!(v, Value::Bool(true)))
        .unwrap_or(false);
    child_ctx.session_messages_handle = match &ctx.agent_entry {
        Some(entry) => Some(std::sync::Arc::clone(&entry.messages)),
        None => {
            let handle: std::sync::Arc<std::sync::Mutex<Vec<_>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            if inherit {
                if let Some(parent) = &ctx.session_messages_handle {
                    *handle.lock().unwrap() = parent.lock().unwrap().clone();
                }
            }
            Some(handle)
        }
    };
    // Sync path has no entry; fresh compact_lock avoids deadlocking on the
    // parent's lock.
    if ctx.agent_entry.is_none() {
        child_ctx.compact_lock_handle = Some(std::sync::Arc::new(tokio::sync::Mutex::new(())));
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

async fn read_flow_source(flow_ref: &str) -> Result<(PathBuf, String), RuntimeError> {
    for path in flow_candidates(flow_ref) {
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

fn flow_candidates(flow_ref: &str) -> Vec<PathBuf> {
    let path = PathBuf::from(flow_ref);
    if path.is_absolute() {
        return vec![path];
    }
    let file_name = if flow_ref.ends_with(".at") {
        flow_ref.to_string()
    } else {
        format!("{flow_ref}.at")
    };
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(
            PathBuf::from(home)
                .join(".config")
                .join("atman")
                .join("commands")
                .join(&file_name),
        );
    }
    out.push(PathBuf::from(file_name));
    out
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
    c.forms = None;
    c.on_memory_recent = None;
    c
}

#[cfg(test)]
mod tests {
    use super::terminal_then_emit;
    use std::cell::Cell;

    #[test]
    fn terminal_transition_happens_before_flow_end_emit() {
        let terminal = Cell::new(false);

        terminal_then_emit(
            || terminal.set(true),
            || assert!(terminal.get(), "FlowEnd emitted before terminal transition"),
        );
    }
}
