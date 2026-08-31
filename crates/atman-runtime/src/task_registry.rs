use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Bash,
    Terminal,
    Flow,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Bash => "Bash",
            TaskKind::Terminal => "Terminal",
            TaskKind::Flow => "Flow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Killing,
    Ok,
    Err,
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTermination {
    Killed,
    Suicide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    Killed { termination: TaskTermination },
    NotFound,
    NotRunning,
    SelfKillRejected,
}

impl TaskStatus {
    pub fn display_label(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Killing => "stopping",
            TaskStatus::Ok => "completed",
            TaskStatus::Err => "failed",
            TaskStatus::Killed => "stopped",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Ok | TaskStatus::Err | TaskStatus::Killed)
    }

    pub fn is_running(self) -> bool {
        matches!(self, TaskStatus::Running | TaskStatus::Killing)
    }
}

pub fn normalize_task_label(value: &str) -> Option<String> {
    crate::message::ToolCallIntent::new(value).map(|label| label.as_str().to_owned())
}

#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub kind: TaskKind,
    pub label: String,
    pub command: Option<String>,
    pub status: TaskStatus,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
    pub source_handle: String,
    pub session_id: String,
    pub workspace_id: Option<String>,
    pub flow_run_id: Option<crate::event::FlowRunId>,
    pub termination: Option<TaskTermination>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDisplay {
    pub label: String,
    pub command: Option<String>,
}

impl From<String> for TaskDisplay {
    fn from(label: String) -> Self {
        Self {
            label,
            command: None,
        }
    }
}

impl From<&str> for TaskDisplay {
    fn from(label: &str) -> Self {
        label.to_owned().into()
    }
}

impl TaskSnapshot {
    pub fn elapsed_ms(&self) -> u64 {
        self.ended_at
            .unwrap_or_else(Instant::now)
            .duration_since(self.started_at)
            .as_millis() as u64
    }

    pub fn is_running(&self) -> bool {
        self.status.is_running()
    }
}

#[derive(Debug, Clone)]
pub enum TaskEvent {
    Registered(TaskSnapshot),
    StatusChanged {
        id: TaskId,
        kind: TaskKind,
        old: TaskStatus,
        new: TaskStatus,
        termination: Option<TaskTermination>,
    },
    Reaped {
        id: TaskId,
    },
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub kind: Option<TaskKind>,
    pub status: Option<TaskStatus>,
    pub session_id: Option<String>,
}

impl TaskFilter {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn running() -> Self {
        Self {
            status: Some(TaskStatus::Running),
            ..Default::default()
        }
    }

    pub fn matches(&self, snap: &TaskSnapshot) -> bool {
        if let Some(k) = self.kind
            && snap.kind != k
        {
            return false;
        }
        if let Some(s) = self.status
            && snap.status != s
        {
            return false;
        }
        if let Some(ref sid) = self.session_id
            && snap.session_id != *sid
        {
            return false;
        }
        true
    }
}

struct TaskEntry {
    snapshot: TaskSnapshot,
    cancel: CancellationToken,
    kill_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// Central management layer for all observable tasks.
///
/// Sub-registries (BgRegistry, TermRegistry, Executor, agent_ctrl) register
/// tasks here on spawn and call `finish` when the task ends. Typed operations
/// (bash.output, term.input, term.capture) stay on the sub-registries and are
/// looked up by `source_handle`.
#[derive(Clone)]
pub struct TaskRegistry {
    inner: Arc<std::sync::Mutex<HashMap<TaskId, TaskEntry>>>,
    event_tx: broadcast::Sender<TaskEvent>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
            event_tx,
        }
    }
}

fn running_snapshot(
    kind: TaskKind,
    display: TaskDisplay,
    source_handle: String,
    session_id: String,
    workspace_id: Option<String>,
    flow_run_id: Option<crate::event::FlowRunId>,
) -> TaskSnapshot {
    TaskSnapshot {
        id: TaskId::now(),
        kind,
        label: display.label,
        command: display.command,
        status: TaskStatus::Running,
        started_at: Instant::now(),
        ended_at: None,
        source_handle,
        session_id,
        workspace_id,
        flow_run_id,
        termination: None,
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        kind: TaskKind,
        display: TaskDisplay,
        source_handle: String,
        session_id: String,
        cancel: CancellationToken,
    ) -> TaskId {
        self.register_snapshot(
            running_snapshot(kind, display, source_handle, session_id, None, None),
            cancel,
            None,
        )
    }

    pub fn register_flow(
        &self,
        label: String,
        source_handle: String,
        session_id: String,
        cancel: CancellationToken,
        workspace_id: Option<String>,
    ) -> TaskId {
        self.register_snapshot(
            running_snapshot(
                TaskKind::Flow,
                label.into(),
                source_handle,
                session_id,
                workspace_id,
                None,
            ),
            cancel,
            None,
        )
    }

    pub fn register_flow_with_run_id(
        &self,
        label: String,
        source_handle: String,
        session_id: String,
        cancel: CancellationToken,
        workspace_id: Option<String>,
        flow_run_id: crate::event::FlowRunId,
    ) -> TaskId {
        self.register_snapshot(
            running_snapshot(
                TaskKind::Flow,
                label.into(),
                source_handle,
                session_id,
                workspace_id,
                Some(flow_run_id),
            ),
            cancel,
            None,
        )
    }

    pub fn register_with_kill_hook(
        &self,
        kind: TaskKind,
        display: TaskDisplay,
        source_handle: String,
        session_id: String,
        cancel: CancellationToken,
        kill_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) -> TaskId {
        self.register_snapshot(
            running_snapshot(kind, display, source_handle, session_id, None, None),
            cancel,
            kill_hook,
        )
    }

    fn register_snapshot(
        &self,
        snapshot: TaskSnapshot,
        cancel: CancellationToken,
        kill_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) -> TaskId {
        let id = snapshot.id.clone();
        let entry = TaskEntry {
            snapshot: snapshot.clone(),
            cancel,
            kill_hook,
        };
        self.inner.lock().unwrap().insert(id.clone(), entry);
        let _ = self.event_tx.send(TaskEvent::Registered(snapshot));
        id
    }

    pub fn lookup(&self, id: &TaskId) -> Option<TaskSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .map(|e| e.snapshot.clone())
    }

    /// Find a task by its source handle (e.g. "bg_1", "term_2", run_id).
    pub fn lookup_by_handle(&self, handle: &str) -> Option<TaskSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .find(|e| e.snapshot.source_handle == handle)
            .map(|e| e.snapshot.clone())
    }

    pub fn list(&self, filter: &TaskFilter) -> Vec<TaskSnapshot> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<TaskSnapshot> = inner
            .values()
            .map(|e| e.snapshot.clone())
            .filter(|s| filter.matches(s))
            .collect();
        out.sort_by_key(|s| s.started_at);
        out
    }

    /// Kill a task at the request of an external operator, such as the TUI.
    ///
    /// Operator actions have no Flow identity, so they can never be mistaken
    /// for a Flow killing itself.
    pub fn kill_from_operator(&self, id: &TaskId) -> KillOutcome {
        self.kill_from(id, None, false)
    }

    pub fn kill_from(
        &self,
        id: &TaskId,
        caller_flow_run_id: Option<&crate::event::FlowRunId>,
        suicide: bool,
    ) -> KillOutcome {
        let mut inner = self.inner.lock().unwrap();
        let Some(entry) = inner.get_mut(id) else {
            return KillOutcome::NotFound;
        };
        if entry.snapshot.status.is_terminal() {
            return KillOutcome::NotRunning;
        }
        let self_targeting = caller_flow_run_id.is_some()
            && entry.snapshot.flow_run_id.as_ref() == caller_flow_run_id;
        if self_targeting && !suicide {
            return KillOutcome::SelfKillRejected;
        }
        let termination = if self_targeting {
            TaskTermination::Suicide
        } else {
            TaskTermination::Killed
        };
        let old_status = entry.snapshot.status;
        let kind = entry.snapshot.kind;
        entry.snapshot.status = TaskStatus::Killing;
        entry.snapshot.termination = Some(termination);
        let cancel = entry.cancel.clone();
        let hook = entry.kill_hook.clone();
        drop(inner);
        cancel.cancel();
        if let Some(hook) = hook {
            hook();
        }
        let _ = self.event_tx.send(TaskEvent::StatusChanged {
            id: id.clone(),
            kind,
            old: old_status,
            new: TaskStatus::Killing,
            termination: Some(termination),
        });
        KillOutcome::Killed { termination }
    }

    /// Transition a task to a terminal status. Called by the owning
    /// sub-registry when the task finishes.
    pub fn finish(&self, id: &TaskId, status: TaskStatus) {
        let mut inner = self.inner.lock().unwrap();
        let Some(entry) = inner.get_mut(id) else {
            return;
        };
        if entry.snapshot.status.is_terminal() {
            return;
        }
        let old = entry.snapshot.status;
        entry.snapshot.status = status;
        entry.snapshot.ended_at = Some(Instant::now());
        let kind = entry.snapshot.kind;
        let termination = entry.snapshot.termination;
        drop(inner);
        let _ = self.event_tx.send(TaskEvent::StatusChanged {
            id: id.clone(),
            kind,
            old,
            new: status,
            termination,
        });
    }

    pub fn reap(&self, id: &TaskId) {
        let mut inner = self.inner.lock().unwrap();
        let should_remove = inner
            .get(id)
            .map(|e| e.snapshot.status.is_terminal())
            .unwrap_or(false);
        if should_remove {
            inner.remove(id);
            drop(inner);
            let _ = self.event_tx.send(TaskEvent::Reaped { id: id.clone() });
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.event_tx.subscribe()
    }

    pub fn running_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.snapshot.status.is_running())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    #[test]
    fn register_and_lookup() {
        let reg = TaskRegistry::new();
        let id = reg.register(
            TaskKind::Bash,
            "cargo build".into(),
            "bg_1".into(),
            "sess".into(),
            cancel(),
        );
        let snap = reg.lookup(&id).expect("found");
        assert_eq!(snap.kind, TaskKind::Bash);
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(snap.ended_at.is_none());
        assert!(snap.command.is_none());
    }

    #[test]
    fn command_is_independent_from_the_user_facing_label() {
        let reg = TaskRegistry::new();
        let id = reg.register(
            TaskKind::Bash,
            TaskDisplay {
                label: "运行项目测试".into(),
                command: Some("cargo test --workspace".into()),
            },
            "bg_1".into(),
            "sess".into(),
            cancel(),
        );
        let snap = reg.lookup(&id).expect("found");
        assert_eq!(snap.label, "运行项目测试");
        assert_eq!(snap.command.as_deref(), Some("cargo test --workspace"));
    }

    #[test]
    fn lookup_by_handle() {
        let reg = TaskRegistry::new();
        let _id = reg.register(
            TaskKind::Terminal,
            "vim".into(),
            "term_1".into(),
            "sess".into(),
            cancel(),
        );
        let snap = reg.lookup_by_handle("term_1").expect("found");
        assert_eq!(snap.kind, TaskKind::Terminal);
        assert!(reg.lookup_by_handle("nope").is_none());
    }

    #[test]
    fn list_filters_by_kind_and_status() {
        let reg = TaskRegistry::new();
        let b1 = reg.register(
            TaskKind::Bash,
            "a".into(),
            "bg_1".into(),
            "s".into(),
            cancel(),
        );
        let _t1 = reg.register(
            TaskKind::Terminal,
            "vim".into(),
            "term_1".into(),
            "s".into(),
            cancel(),
        );
        let _b2 = reg.register(
            TaskKind::Bash,
            "ls".into(),
            "bg_2".into(),
            "s".into(),
            cancel(),
        );

        let bash_only = reg.list(&TaskFilter {
            kind: Some(TaskKind::Bash),
            ..Default::default()
        });
        assert_eq!(bash_only.len(), 2);

        reg.finish(&b1, TaskStatus::Ok);
        let running = reg.list(&TaskFilter::running());
        assert_eq!(running.len(), 2);
    }

    #[test]
    fn kill_cancels_token() {
        let reg = TaskRegistry::new();
        let tok = cancel();
        let id = reg.register(
            TaskKind::Bash,
            "x".into(),
            "bg".into(),
            "s".into(),
            tok.clone(),
        );
        assert_eq!(
            reg.kill_from_operator(&id),
            KillOutcome::Killed {
                termination: TaskTermination::Killed
            }
        );
        assert!(tok.is_cancelled());
    }

    #[test]
    fn self_kill_requires_suicide_confirmation() {
        let reg = TaskRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        let token = cancel();
        let id = reg.register_flow_with_run_id(
            "flow".into(),
            "agent".into(),
            "s".into(),
            token.clone(),
            None,
            run_id.clone(),
        );

        assert_eq!(
            reg.kill_from(&id, Some(&run_id), false),
            KillOutcome::SelfKillRejected
        );
        assert!(!token.is_cancelled());
        assert_eq!(reg.lookup(&id).unwrap().status, TaskStatus::Running);
    }

    #[test]
    fn confirmed_self_kill_records_suicide_cause() {
        let reg = TaskRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        let token = cancel();
        let id = reg.register_flow_with_run_id(
            "flow".into(),
            "agent".into(),
            "s".into(),
            token.clone(),
            None,
            run_id.clone(),
        );

        assert_eq!(
            reg.kill_from(&id, Some(&run_id), true),
            KillOutcome::Killed {
                termination: TaskTermination::Suicide
            }
        );
        assert!(token.is_cancelled());
        assert_eq!(
            reg.lookup(&id).unwrap().termination,
            Some(TaskTermination::Suicide)
        );
    }

    #[test]
    fn operator_kill_is_not_classified_as_suicide() {
        let reg = TaskRegistry::new();
        let target_run_id = crate::event::FlowRunId::now();
        let id = reg.register_flow_with_run_id(
            "flow".into(),
            "agent".into(),
            "s".into(),
            cancel(),
            None,
            target_run_id,
        );

        assert_eq!(
            reg.kill_from_operator(&id),
            KillOutcome::Killed {
                termination: TaskTermination::Killed
            }
        );
        assert_eq!(
            reg.lookup(&id).unwrap().termination,
            Some(TaskTermination::Killed)
        );
    }

    #[test]
    fn another_flow_can_kill_without_suicide_confirmation() {
        let reg = TaskRegistry::new();
        let target_run_id = crate::event::FlowRunId::now();
        let caller_run_id = crate::event::FlowRunId::now();
        let id = reg.register_flow_with_run_id(
            "flow".into(),
            "agent".into(),
            "s".into(),
            cancel(),
            None,
            target_run_id,
        );

        assert_eq!(
            reg.kill_from(&id, Some(&caller_run_id), false),
            KillOutcome::Killed {
                termination: TaskTermination::Killed
            }
        );
    }

    #[test]
    fn kill_returns_false_for_terminal() {
        let reg = TaskRegistry::new();
        let id = reg.register(
            TaskKind::Bash,
            "x".into(),
            "bg".into(),
            "s".into(),
            cancel(),
        );
        reg.finish(&id, TaskStatus::Ok);
        assert_eq!(reg.kill_from_operator(&id), KillOutcome::NotRunning);
    }

    #[test]
    fn finish_is_idempotent() {
        let reg = TaskRegistry::new();
        let id = reg.register(
            TaskKind::Bash,
            "x".into(),
            "bg".into(),
            "s".into(),
            cancel(),
        );
        reg.finish(&id, TaskStatus::Ok);
        reg.finish(&id, TaskStatus::Err);
        let snap = reg.lookup(&id).unwrap();
        assert_eq!(snap.status, TaskStatus::Ok);
    }

    #[test]
    fn reap_removes_terminal_only() {
        let reg = TaskRegistry::new();
        let id = reg.register(
            TaskKind::Bash,
            "x".into(),
            "bg".into(),
            "s".into(),
            cancel(),
        );
        reg.reap(&id);
        assert!(reg.lookup(&id).is_some());
        reg.finish(&id, TaskStatus::Ok);
        reg.reap(&id);
        assert!(reg.lookup(&id).is_none());
    }

    #[test]
    fn subscribe_receives_registered_event() {
        let reg = TaskRegistry::new();
        let mut rx = reg.subscribe();
        let _id = reg.register(
            TaskKind::Bash,
            "x".into(),
            "bg".into(),
            "s".into(),
            cancel(),
        );
        let ev = rx.try_recv().expect("got event");
        match ev {
            TaskEvent::Registered(s) => assert_eq!(s.kind, TaskKind::Bash),
            _ => panic!("wrong event"),
        }
    }

    #[test]
    fn subscribe_receives_status_changed() {
        let reg = TaskRegistry::new();
        let mut rx = reg.subscribe();
        let id = reg.register(
            TaskKind::Bash,
            "x".into(),
            "bg".into(),
            "s".into(),
            cancel(),
        );
        let _ = rx.try_recv();
        reg.finish(&id, TaskStatus::Ok);
        let ev = rx.try_recv().expect("got status event");
        match ev {
            TaskEvent::StatusChanged { new, .. } => assert_eq!(new, TaskStatus::Ok),
            _ => panic!("wrong event"),
        }
    }

    #[test]
    fn filter_matches_combines() {
        let snap = TaskSnapshot {
            id: TaskId::now(),
            kind: TaskKind::Terminal,
            label: "vim".into(),
            command: None,
            status: TaskStatus::Running,
            started_at: Instant::now(),
            ended_at: None,
            source_handle: "term_1".into(),
            session_id: "sess_a".into(),
            workspace_id: None,
            flow_run_id: None,
            termination: None,
        };
        let f = TaskFilter {
            kind: Some(TaskKind::Terminal),
            status: Some(TaskStatus::Running),
            session_id: Some("sess_a".into()),
        };
        assert!(f.matches(&snap));

        let f2 = TaskFilter {
            kind: Some(TaskKind::Bash),
            ..Default::default()
        };
        assert!(!f2.matches(&snap));
    }

    #[test]
    fn task_status_has_stable_display_labels() {
        assert_eq!(TaskStatus::Running.display_label(), "running");
        assert_eq!(TaskStatus::Killing.display_label(), "stopping");
        assert_eq!(TaskStatus::Ok.display_label(), "completed");
        assert_eq!(TaskStatus::Err.display_label(), "failed");
        assert_eq!(TaskStatus::Killed.display_label(), "stopped");
    }

    #[test]
    fn task_label_normalization_matches_tool_intent_bounds() {
        assert_eq!(
            normalize_task_label("  检查   当前状态  ").as_deref(),
            Some("检查 当前状态")
        );
        assert!(normalize_task_label(" \n\t ").is_none());
        assert_eq!(
            normalize_task_label(&"x".repeat(121))
                .unwrap()
                .chars()
                .count(),
            120
        );
    }
}
