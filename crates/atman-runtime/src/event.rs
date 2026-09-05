use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct FlowRunId(pub Uuid);

impl FlowRunId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for FlowRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);

impl TurnId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity of a message context, independent of its turns and execution runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ContextId(pub Uuid);

impl ContextId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for ContextId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity of one context compaction attempt from start through terminal state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct CompactionOperationId(pub Uuid);

impl CompactionOperationId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for CompactionOperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Selection applied to the inherited active window, without changing raw history.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextInheritance {
    #[default]
    Full,
    CompleteToolPairs,
}

/// A fixed event-log boundary in an explicitly selected message context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextBase {
    LegacyRoot {
        through_seq: u64,
    },
    Context {
        context_id: ContextId,
        through_seq: u64,
    },
}

impl ContextBase {
    pub fn context_id(&self) -> Option<&ContextId> {
        match self {
            Self::LegacyRoot { .. } => None,
            Self::Context { context_id, .. } => Some(context_id),
        }
    }

    pub fn through_seq(&self) -> u64 {
        match self {
            Self::LegacyRoot { through_seq } | Self::Context { through_seq, .. } => *through_seq,
        }
    }
}

/// An event with sequence, timestamp, and optional message-context identity.
/// Unscoped events retain the legacy flat JSONL representation.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub context_id: Option<ContextId>,
    pub event: Event,
}

impl EventEnvelope {
    pub fn new(seq: u64, event: Event) -> Self {
        let ts = chrono::Utc::now();
        Self {
            seq,
            ts,
            context_id: None,
            event,
        }
    }

    pub(crate) fn from_json_value(value: serde_json::Value) -> serde_json::Result<Self> {
        let seq = value.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let ts = value
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let context_id = value
            .get("context_id")
            .map(|id| serde_json::from_value::<Option<ContextId>>(id.clone()))
            .transpose()?
            .flatten();
        let event = serde_json::from_value(value)?;
        Ok(Self {
            seq,
            ts,
            context_id,
            event,
        })
    }
}

impl serde::Serialize for EventEnvelope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.event).map_err(serde::ser::Error::custom)?;
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert("seq".into(), serde_json::Value::Number(self.seq.into()));
            map.insert("ts".into(), serde_json::Value::String(self.ts.to_rfc3339()));
            if let Some(id) = &self.context_id {
                map.insert(
                    "context_id".into(),
                    serde_json::Value::String(id.to_string()),
                );
            }
        }
        value.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for EventEnvelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_json_value(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The envelope identifies the new context; no base means empty history.
    ContextCreated {
        base: Option<ContextBase>,
        inheritance: ContextInheritance,
    },
    /// Selects the envelope's context as the default for an accepted turn.
    ContextHeadSelected {
        turn_id: TurnId,
    },
    FlowStart {
        run_id: FlowRunId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        #[serde(default)]
        flow_name: String,
        #[serde(default)]
        parent_run_id: Option<FlowRunId>,
        #[serde(default)]
        parent_node_id: Option<String>,
        #[serde(default)]
        spawned: bool,
    },
    FlowEnd {
        run_id: FlowRunId,
        flow_name: String,
        status: FlowStatus,
    },
    RunCancelRequested {
        run_id: FlowRunId,
    },
    GenerationReconciled {
        daemon_generation: String,
        reason: String,
        lost_runs: Vec<FlowRunId>,
        orphaned_resources: Vec<String>,
    },
    WorkspaceLifecycle {
        run_id: FlowRunId,
        workspace_id: String,
        path: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cleanup_error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reconciliation_reason: Option<String>,
    },
    TaskLifecycle {
        task_id: crate::task_registry::TaskId,
        kind: crate::task_registry::TaskKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<FlowRunId>,
        source_handle: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        status: crate::task_registry::TaskStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        termination: Option<crate::task_registry::TaskTermination>,
    },
    TaskReaped {
        task_id: crate::task_registry::TaskId,
    },
    LlmCall {
        model: String,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_plan_id: Option<crate::context_plan::ContextPlanId>,
        /// Whether this request read the bound message context, rather than explicit input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        managed_context: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_epoch: Option<crate::context_plan::ContextEpoch>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_tokens: Option<crate::context_plan::ContextTokenLanes>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_source: Option<crate::context_plan::TokenUsageSource>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_call_purpose: Option<crate::context_plan::ContextCallPurpose>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_call_identity: Option<crate::context_plan::ContextCallIdentity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_cache: Option<crate::context_plan::ContextCacheObservation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assistant_tool_batch_width: Option<u64>,
        #[serde(default)]
        usage: crate::provider::TokenUsage,
        #[serde(default)]
        wallclock_ms: u64,
        #[serde(default)]
        ttft_ms: Option<u64>,
        #[serde(default)]
        tokens_per_second: Option<f64>,
        #[serde(default)]
        status: LlmCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<crate::event::FlowRunId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node_id: Option<String>,
    },
    TurnStart {
        turn_id: TurnId,
    },
    TurnEnd {
        turn_id: TurnId,
    },
    UserMsg {
        turn_id: TurnId,
        #[serde(default)]
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
    },
    AssistantMsg {
        turn_id: TurnId,
        #[serde(default)]
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
    },
    ToolResultMsg {
        turn_id: TurnId,
        #[serde(default)]
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
    },
    ToolResultMetrics {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        tool_use_id: String,
        raw_bytes: u64,
        excerpt_bytes: u64,
        truncated: bool,
    },
    DiffPreview {
        #[serde(default)]
        turn_id: Option<TurnId>,
        #[serde(default)]
        flow_run_id: Option<FlowRunId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        title: String,
        #[serde(default)]
        old_content: Option<String>,
        #[serde(default)]
        new_content: Option<String>,
        #[serde(default)]
        unified_diff: Option<String>,
    },
    FileEditApplied {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        tool_name: String,
        path: String,
        metrics: crate::activity::EditMetrics,
    },
    CompactionStarted {
        operation_id: CompactionOperationId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        range_start: u64,
        range_end: u64,
        compacted_count: usize,
        before_tokens: u64,
    },
    CompactionSummary {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<CompactionOperationId>,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        range_start: u64,
        range_end: u64,
        compacted_count: usize,
        before_tokens: u64,
        after_tokens: u64,
        summary: String,
    },
    CompactionFailed {
        operation_id: CompactionOperationId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        range_start: u64,
        range_end: u64,
        compacted_count: usize,
        before_tokens: u64,
        reason: String,
    },
    SystemMsg {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
    },
    UserInject {
        turn_id: TurnId,
        injection: crate::injection::Injection,
        /// Rendered steering admitted to context with this consumption update.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_message: Option<crate::message::Message>,
    },
    ContentFilterHit {
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        provider: String,
        model: String,
        category: String,
        action: String,
    },
    ContextCompact {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        before_tokens: u64,
        after_tokens: u64,
        compacted_range_start: u64,
        compacted_range_end: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_msg_seq: Option<u64>,
    },
    Checkpoint {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        messages: Vec<crate::message::Message>,
        window_tokens: u64,
    },
    ContextTruncated {
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        original_chars: u64,
        result_chars: u64,
        dropped_chars: u64,
        budget_tokens: u64,
    },
    WatchWarn {
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        target: String,
        trigger: String,
        message: String,
    },
    LlmPartialCall {
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        model: String,
        provider: String,
        tokens_before_abort: u64,
        restart_reason: String,
    },
    PendingPrompt {
        prompt_id: uuid::Uuid,
        kind: String,
        payload: serde_json::Value,
    },
    PromptResolved {
        prompt_id: uuid::Uuid,
        answer: serde_json::Value,
    },
    FormRequested {
        form: crate::form::PendingForm,
    },
    FormResolved {
        form_id: String,
        run_id: FlowRunId,
        submission: crate::form::FormSubmission,
        #[serde(default)]
        abandoned: bool,
    },
    CompactReviewRequested {
        review: crate::session::PendingCompactReview,
    },
    CompactReviewResolved {
        review_id: String,
        decision: crate::session::CompactReviewDecision,
        #[serde(default)]
        abandoned: bool,
    },
    FlowGraph {
        run_id: FlowRunId,
        graph: crate::nodegraph::FlowGraph,
    },
    FlowNodeStart {
        run_id: FlowRunId,
        node_id: String,
        #[serde(default = "default_replay_node_kind")]
        kind: crate::nodegraph::NodeKind,
        #[serde(default)]
        label: String,
        #[serde(default)]
        parent_node_id: Option<String>,
    },
    FlowNodeEnd {
        run_id: FlowRunId,
        node_id: String,
        #[serde(default)]
        status: FlowNodeStatus,
        #[serde(default)]
        output_preview: Option<String>,
    },
    ToolNode {
        run_id: FlowRunId,
        parent_node_id: String,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
    },
    AttachmentDegraded {
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        #[serde(flatten)]
        patch: crate::message::AttachmentPatch,
    },
    ToolPendingApproval {
        run_id: FlowRunId,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        level: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    ToolApproved {
        run_id: FlowRunId,
        tool_use_id: String,
        decided_by: String,
    },
    ToolDenied {
        run_id: FlowRunId,
        tool_use_id: String,
        reason: String,
    },
    PermissionRequestCreated {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestTargeted {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestDeferred {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestApproved {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestDenied {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestCancelled {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionGroupCreated {
        payload: crate::permission_audit::PermissionGroupAudit,
    },
    PermissionGroupUpdated {
        payload: crate::permission_audit::PermissionGroupAudit,
    },
    PermissionGroupResolved {
        payload: crate::permission_audit::PermissionGroupAudit,
    },
    PermissionGrantCreated {
        payload: crate::permission_audit::PermissionGrantAudit,
    },
    PermissionGrantExpired {
        payload: crate::permission_audit::PermissionGrantAudit,
    },
    UnrestrictedExecution {
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    /// Persisted when a terminal's reader loop exits (normal exit or kill).
    /// Carries the last screen state so TUI restore can show it instead of
    /// the empty placeholder from the spawn-time ToolResultMsg.
    TerminalFinalState {
        handle: String,
        screen: crate::tools::term::TerminalScreen,
        state: crate::tools::term::TermStateSnapshot,
    },
    MermaidDiagram {
        source: String,
    },
}

impl Event {
    /// Returns the persisted message and its run owner, including consumed steering.
    pub fn context_message(&self) -> Option<(&crate::message::Message, Option<&FlowRunId>)> {
        match self {
            Self::UserMsg {
                message,
                flow_run_id,
                ..
            }
            | Self::AssistantMsg {
                message,
                flow_run_id,
                ..
            }
            | Self::ToolResultMsg {
                message,
                flow_run_id,
                ..
            }
            | Self::SystemMsg {
                message,
                flow_run_id,
                ..
            } => Some((message, flow_run_id.as_ref())),
            Self::UserInject {
                injection,
                context_message: Some(message),
                ..
            } if injection.state == crate::injection::InjectionState::Injected => {
                Some((message, injection.flow_run_id.as_ref()))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FlowNodeStatus {
    #[default]
    Ok,
    Err,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowStatus {
    Ok,
    Errored { message: String },
    Cancelled,
}

impl FlowStatus {
    pub(crate) fn for_result(result: &crate::tool::ToolResult) -> Self {
        match result {
            Err(crate::error::RuntimeError::Cancelled(_))
            | Ok(crate::value::Value::Err(crate::error::RuntimeError::Cancelled(_))) => {
                Self::Cancelled
            }
            Err(error) => Self::errored(error.to_string()),
            Ok(_) => Self::Ok,
        }
    }

    pub fn errored(msg: impl Into<String>) -> Self {
        Self::Errored {
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmCallStatus {
    #[default]
    Ok,
    Errored {
        message: String,
    },
}

fn default_replay_node_kind() -> crate::nodegraph::NodeKind {
    crate::nodegraph::NodeKind::UserConfirm
}

impl LlmCallStatus {
    pub fn errored(msg: impl Into<String>) -> Self {
        Self::Errored {
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NodeEvent {
    LlmChunk {
        text: String,
        cumulative_tokens: u64,
    },
    ThinkingChunk {
        text: String,
    },
    ToolCallDraft {
        index: usize,
        call_id: String,
        name: String,
        arguments_delta: String,
    },
    LlmDone {
        total_tokens: u64,
    },
    ToolStdoutLine {
        line: String,
    },
    ToolStderrLine {
        line: String,
    },
    ToolDone {
        exit: i32,
    },
}

pub struct Observable<T> {
    pub output: crate::tool::BoxFut<'static, Result<T, crate::error::RuntimeError>>,
    pub events: broadcast::Receiver<NodeEvent>,
    pub cancel: CancellationToken,
}

const EVENT_SUBSCRIBER_BUFFER: usize = 2_048;

#[derive(Clone)]
pub struct EventSink {
    context_id: Option<ContextId>,
    events: Arc<Mutex<Vec<EventEnvelope>>>,
    event_tx: broadcast::Sender<EventEnvelope>,
    forwarder: Option<mpsc::UnboundedSender<EventEnvelope>>,
    seq_counter: Arc<std::sync::atomic::AtomicU64>,
    published_seq: Arc<std::sync::atomic::AtomicU64>,
    redactor: Option<Arc<crate::redact::Redactor>>,
}

/// Serializes related records against other emitters and shared-log readers.
/// Records are still individually delivered and persisted; this is not a disk transaction.
pub(crate) struct EventBatch<'a> {
    sink: &'a EventSink,
    events: std::sync::MutexGuard<'a, Vec<EventEnvelope>>,
}

impl EventBatch<'_> {
    pub(crate) fn records(&self) -> &[EventEnvelope] {
        &self.events
    }

    pub(crate) fn emit(&mut self, event: Event) -> EventEnvelope {
        let next = self
            .sink
            .seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let mut envelope = EventEnvelope::new(next, event);
        envelope.context_id.clone_from(&self.sink.context_id);
        if let Some(tx) = &self.sink.forwarder {
            let _ = tx.send(envelope.clone());
        }
        self.events.push(envelope.clone());
        let _ = self.sink.event_tx.send(envelope.clone());
        self.sink
            .published_seq
            .store(next, std::sync::atomic::Ordering::Release);
        envelope
    }
}

impl Default for EventSink {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_SUBSCRIBER_BUFFER);
        Self {
            context_id: None,
            events: Arc::new(Mutex::new(Vec::new())),
            event_tx,
            forwarder: None,
            seq_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            published_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            redactor: None,
        }
    }
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_forwarder(mut self, tx: mpsc::UnboundedSender<EventEnvelope>) -> Self {
        self.forwarder = Some(tx);
        self
    }

    pub fn with_redactor(mut self, redactor: Arc<crate::redact::Redactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    /// Scopes emitted envelopes without changing sibling sinks or subscriptions.
    /// Clones share the complete event log and its sequence, not a filtered view.
    pub fn with_context(mut self, context_id: ContextId) -> Self {
        self.context_id = Some(context_id);
        self
    }

    pub fn context_id(&self) -> Option<&ContextId> {
        self.context_id.as_ref()
    }

    // Best-effort peek for anchor labels. NOT reserved: two peekers see the same value.
    // Safe while evaluator dispatch remains sequential per flow. Parallel dispatch must
    // reserve sequence numbers instead.
    pub fn next_seq_peek(&self) -> u64 {
        self.seq_counter.load(std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// Highest sequence published to subscribers or restored from the event log.
    /// Excludes reservations and in-flight emission; does not imply a disk flush.
    pub fn published_seq(&self) -> u64 {
        self.published_seq
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn restore_seq(&self, last_seq: u64) {
        let _events = self.events.lock().expect("event sink poisoned");
        self.seq_counter
            .store(last_seq, std::sync::atomic::Ordering::SeqCst);
        self.published_seq
            .store(last_seq, std::sync::atomic::Ordering::Release);
    }

    // Atomic reserve for the future parallel-dispatch case: returns a seq value that
    // no other reservation can obtain, at the cost of advancing the counter even if
    // the caller never emits (a hole in seq numbering). Not used yet; kept ready.
    pub fn reserve_seq(&self) -> u64 {
        let _events = self.events.lock().expect("event sink poisoned");
        self.seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    pub fn emit_returning_seq(&self, event: Event) -> u64 {
        self.emit_returning_envelope(event).seq
    }

    pub fn emit_returning_envelope(&self, event: Event) -> EventEnvelope {
        self.batch().emit(event)
    }

    pub(crate) fn batch(&self) -> EventBatch<'_> {
        EventBatch {
            sink: self,
            events: self.events.lock().expect("event sink poisoned"),
        }
    }

    pub fn emit(&self, event: Event) {
        self.emit_returning_envelope(event);
    }

    pub fn events_handle(&self) -> Arc<Mutex<Vec<EventEnvelope>>> {
        self.events.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.event_tx.subscribe()
    }

    pub fn redactor(&self) -> Option<Arc<crate::redact::Redactor>> {
        self.redactor.clone()
    }

    pub fn drain(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().expect("event sink poisoned"))
            .into_iter()
            .map(|envelope| envelope.event)
            .collect()
    }

    pub fn snapshot(&self) -> Vec<Event> {
        self.events
            .lock()
            .expect("event sink poisoned")
            .iter()
            .map(|envelope| envelope.event.clone())
            .collect()
    }

    pub fn snapshot_envelopes(&self) -> Vec<EventEnvelope> {
        self.events
            .lock()
            .expect("event sink poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_steering_replays_captured_text_without_reinterpreting_legacy_updates() {
        let run_id = FlowRunId::now();
        let mut injection = crate::injection::Injection::with_level_for_run(
            TurnId::now(),
            "source text",
            crate::injection::InjectionLevel::L1Nudge,
            None,
            Some(run_id.clone()),
        );
        injection.state = crate::injection::InjectionState::Injected;
        let message =
            crate::message::Message::user_text(injection.turn_id.clone(), "captured rendering");
        let event = Event::UserInject {
            turn_id: injection.turn_id.clone(),
            injection,
            context_message: Some(message.clone()),
        };
        let mut value = serde_json::to_value(&event).unwrap();
        let restored: Event = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(restored.context_message(), Some((&message, Some(&run_id))));
        value.as_object_mut().unwrap().remove("context_message");
        let legacy: Event = serde_json::from_value(value).unwrap();
        assert!(legacy.context_message().is_none());
    }

    #[test]
    fn flow_start_serializes_parent_linkage() {
        let parent = FlowRunId::now();
        let turn_id = TurnId::now();
        let ev = Event::FlowStart {
            run_id: FlowRunId::now(),
            turn_id: Some(turn_id.clone()),
            flow_name: "child".into(),
            parent_run_id: Some(parent.clone()),
            parent_node_id: Some("stmt_3".into()),
            spawned: false,
        };
        let mut v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "flow_start");
        assert_eq!(v["parent_run_id"], serde_json::json!(parent.0.to_string()));
        assert_eq!(v["parent_node_id"], "stmt_3");
        assert_eq!(v["turn_id"], serde_json::json!(turn_id.0));
        assert_eq!(
            crate::event_writer::extract_anchors(&ev).0,
            Some(turn_id.to_string())
        );
        v.as_object_mut().unwrap().remove("turn_id");
        assert!(matches!(
            serde_json::from_value::<Event>(v).unwrap(),
            Event::FlowStart { turn_id: None, .. }
        ));
    }

    #[test]
    fn flow_node_start_carries_parent_node_id() {
        let ev = Event::FlowNodeStart {
            run_id: FlowRunId::now(),
            node_id: "stmt_1.branch[0]".into(),
            kind: crate::nodegraph::NodeKind::UserConfirm,
            label: "fanout".into(),
            parent_node_id: Some("stmt_1".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "flow_node_start");
        assert_eq!(v["parent_node_id"], "stmt_1");
    }

    #[test]
    fn tool_node_serializes_all_fields() {
        let run_id = FlowRunId::now();
        let ev = Event::ToolNode {
            run_id: run_id.clone(),
            parent_node_id: "stmt_2".into(),
            tool_use_id: "tu_abc".into(),
            tool_name: "fs.read".into(),
            args_preview: "{\"path\":\"a.rs\"}".into(),
            call_intent: crate::message::ToolCallIntent::new("Inspect source"),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "tool_node");
        assert_eq!(v["run_id"], run_id.0.to_string());
        assert_eq!(v["parent_node_id"], "stmt_2");
        assert_eq!(v["tool_use_id"], "tu_abc");
        assert_eq!(v["tool_name"], "fs.read");
        assert_eq!(v["args_preview"], "{\"path\":\"a.rs\"}");
        assert_eq!(v["call_intent"], "Inspect source");
    }

    #[test]
    fn tool_result_metrics_serialize_raw_and_excerpt_sizes() {
        let ev = Event::ToolResultMetrics {
            turn_id: TurnId::now(),
            flow_run_id: Some(FlowRunId::now()),
            tool_use_id: "call-1".into(),
            raw_bytes: 10_000,
            excerpt_bytes: 1_000,
            truncated: true,
        };
        let value = serde_json::to_value(ev).unwrap();

        assert_eq!(value["type"], "tool_result_metrics");
        assert_eq!(value["tool_use_id"], "call-1");
        assert_eq!(value["raw_bytes"], 10_000);
        assert_eq!(value["excerpt_bytes"], 1_000);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn seq_and_set_seq_cover_tool_node() {
        let _ev = Event::ToolNode {
            run_id: FlowRunId::now(),
            parent_node_id: "s".into(),
            tool_use_id: "t".into(),
            tool_name: "n".into(),
            args_preview: "{}".into(),
            call_intent: None,
        };
    }

    #[test]
    fn attachment_degraded_serializes_all_fields() {
        let turn = TurnId::now();
        let flow = FlowRunId::now();
        let ev = Event::AttachmentDegraded {
            turn_id: Some(turn.clone()),
            flow_run_id: Some(flow.clone()),
            patch: crate::message::AttachmentPatch {
                target: crate::message::AttachmentTarget::Legacy {
                    message_seq: 42,
                    part_index: 1,
                },
                file_basename: "photo.png".into(),
                reason: "image_too_large".into(),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "attachment_degraded");
        assert_eq!(v["message_seq"], 42);
        assert_eq!(v["part_index"], 1);
        assert_eq!(v["file_basename"], "photo.png");
        assert_eq!(v["reason"], "image_too_large");
        assert_eq!(v["turn_id"], serde_json::json!(turn.0.to_string()));
        assert_eq!(v["flow_run_id"], serde_json::json!(flow.0.to_string()));
    }

    #[test]
    fn attachment_target_addresses_are_exclusive_and_round_trip_in_envelopes() {
        let id = uuid::Uuid::now_v7();
        for (address, valid) in [
            (serde_json::json!({"part_id": id}), true),
            (serde_json::json!({"message_seq": 1, "part_index": 0}), true),
            (
                serde_json::json!({"part_id": id, "message_seq": 1, "part_index": 0}),
                false,
            ),
            (serde_json::json!({"part_id": id, "message_seq": 1}), false),
            (
                serde_json::json!({"part_id": null, "message_seq": 1, "part_index": 0}),
                false,
            ),
            (
                serde_json::json!({"part_id": "invalid", "message_seq": 1, "part_index": 0}),
                false,
            ),
            (serde_json::json!({"part_id": null}), false),
            (serde_json::json!({"part_id": "invalid"}), false),
            (serde_json::json!({"message_seq": 1}), false),
            (serde_json::json!({"part_index": 0}), false),
            (
                serde_json::json!({"message_seq": -1, "part_index": 0}),
                false,
            ),
            (
                serde_json::json!({"message_seq": 1, "part_index": null}),
                false,
            ),
            (serde_json::json!({}), false),
        ] {
            let mut value = serde_json::json!({
                "type": "attachment_degraded", "seq": 9, "ts": chrono::Utc::now().to_rfc3339(),
                "context_id": ContextId::now(), "turn_id": null, "flow_run_id": null,
                "file_basename": "image.png", "reason": "unreadable",
            });
            value
                .as_object_mut()
                .unwrap()
                .extend(address.as_object().unwrap().clone());
            let decoded = serde_json::from_value::<EventEnvelope>(value.clone());
            assert_eq!(decoded.is_ok(), valid, "{value}");
            if let Ok(envelope) = decoded {
                assert_eq!(serde_json::to_value(envelope).unwrap(), value);
            }
        }
    }

    #[test]
    fn tool_pending_approval_round_trip() {
        let ev = Event::ToolPendingApproval {
            run_id: FlowRunId::now(),
            tool_use_id: "tu1".into(),
            tool_name: "fs.write".into(),
            args_preview: "{}".into(),
            level: "approve".into(),
            preview: None,
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "tool_pending_approval");
        assert_eq!(v["tool_use_id"], "tu1");
        assert_eq!(v["level"], "approve");
    }

    #[test]
    fn seq_and_set_seq_cover_approval_variants() {
        let rid = FlowRunId::now();
        for _ev in [
            Event::ToolPendingApproval {
                run_id: rid.clone(),
                tool_use_id: "t".into(),
                tool_name: "n".into(),
                args_preview: "{}".into(),
                level: "approve".into(),
                preview: None,
            },
            Event::ToolApproved {
                run_id: rid.clone(),
                tool_use_id: "t".into(),
                decided_by: "user".into(),
            },
            Event::ToolDenied {
                run_id: rid.clone(),
                tool_use_id: "t".into(),
                reason: "no".into(),
            },
        ] {}
    }

    #[test]
    fn compaction_summary_serializes_all_fields() {
        let ev = Event::CompactionSummary {
            operation_id: Some(CompactionOperationId::now()),
            session_id: "sess".into(),
            flow_run_id: None,
            range_start: 2,
            range_end: 8,
            compacted_count: 7,
            before_tokens: 1000,
            after_tokens: 250,
            summary: "gist".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "compaction_summary");
        assert_eq!(v["session_id"], "sess");
        assert_eq!(v["range_start"], 2);
        assert_eq!(v["range_end"], 8);
        assert_eq!(v["compacted_count"], 7);
        assert_eq!(v["before_tokens"], 1000);
        assert_eq!(v["after_tokens"], 250);
        assert_eq!(v["summary"], "gist");
    }

    #[test]
    fn seq_and_set_seq_cover_compaction_summary() {
        let _ev = Event::CompactionSummary {
            operation_id: None,
            session_id: "sess".into(),
            flow_run_id: None,
            range_start: 0,
            range_end: 1,
            compacted_count: 2,
            before_tokens: 10,
            after_tokens: 3,
            summary: String::new(),
        };
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let env = EventEnvelope::new(
            42,
            Event::UserMsg {
                turn_id: TurnId::now(),
                flow_run_id: None,
                message: crate::message::Message::user_text(TurnId::now(), "hello"),
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
        assert!(matches!(back.event, Event::UserMsg { .. }));
    }

    #[test]
    fn context_scope_is_typed_and_absent_from_legacy_json() {
        let envelope = EventEnvelope::new(
            42,
            Event::RunCancelRequested {
                run_id: FlowRunId::now(),
            },
        );
        let mut value = serde_json::to_value(&envelope).unwrap();
        assert!(value.get("context_id").is_none());
        for scope in [None, Some(serde_json::Value::Null)] {
            if let Some(scope) = scope {
                value["context_id"] = scope;
            }
            let restored: EventEnvelope = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(restored.context_id, None);
            assert_eq!(restored.ts, envelope.ts);
        }
        for invalid in [
            serde_json::json!("invalid"),
            serde_json::json!(7),
            serde_json::json!({}),
        ] {
            value["context_id"] = invalid;
            assert!(serde_json::from_value::<EventEnvelope>(value.clone()).is_err());
        }
    }

    #[test]
    fn scoped_sinks_share_ordered_delivery_without_changing_sibling_identity() {
        let (tx, mut forwarded) = mpsc::unbounded_channel();
        let sink = EventSink::new().with_forwarder(tx);
        let left_id = ContextId::now();
        let right_id = ContextId::now();
        let left = sink.clone().with_context(left_id.clone());
        let right = left.clone().with_context(right_id.clone());
        let mut subscribed = right.subscribe();
        std::thread::scope(|scope| {
            for source in [&sink, &left, &right] {
                scope.spawn(move || {
                    for _ in 0..8 {
                        source.emit(Event::RunCancelRequested {
                            run_id: FlowRunId::now(),
                        });
                    }
                });
            }
        });
        let events = sink.snapshot_envelopes();
        assert_eq!(sink.published_seq(), 24);
        assert_eq!(left.published_seq(), 24);
        for id in [None, Some(left_id), Some(right_id)] {
            assert_eq!(
                events.iter().filter(|event| event.context_id == id).count(),
                8
            );
        }
        for (index, envelope) in events.iter().enumerate() {
            assert_eq!(envelope.seq, index as u64 + 1);
            let expected = serde_json::to_value(envelope).unwrap();
            for delivered in [
                forwarded.try_recv().unwrap(),
                subscribed.try_recv().unwrap(),
            ] {
                assert_eq!(serde_json::to_value(delivered).unwrap(), expected);
            }
            let restored: EventEnvelope = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(restored).unwrap(), expected);
        }
        assert!(forwarded.try_recv().is_err());
        assert!(subscribed.try_recv().is_err());
    }

    #[test]
    fn batches_exclude_shared_log_readers_and_other_emitters() {
        let (tx, mut forwarded) = mpsc::unbounded_channel();
        let sink = EventSink::new().with_forwarder(tx);
        let left = sink.clone().with_context(ContextId::now());
        let right = sink.clone().with_context(ContextId::now());
        let mut subscribed = sink.subscribe();
        {
            let _batch = left.batch();
            assert!(matches!(
                sink.events.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
        }
        std::thread::scope(|scope| {
            for source in [&sink, &left, &right] {
                scope.spawn(move || {
                    for _ in 0..8 {
                        let run_id = FlowRunId::now();
                        let mut batch = source.batch();
                        batch.emit(Event::RunCancelRequested {
                            run_id: run_id.clone(),
                        });
                        std::thread::yield_now();
                        batch.emit(Event::RunCancelRequested { run_id });
                    }
                });
            }
        });
        let events = sink.snapshot_envelopes();
        assert_eq!(events.len(), 48);
        assert_eq!(sink.published_seq(), 48);
        for pair in events.chunks_exact(2) {
            assert_eq!(pair[0].context_id, pair[1].context_id);
            assert_eq!(
                serde_json::to_value(&pair[0].event).unwrap(),
                serde_json::to_value(&pair[1].event).unwrap()
            );
        }
        for (index, envelope) in events.iter().enumerate() {
            assert_eq!(envelope.seq, index as u64 + 1);
            let expected = serde_json::to_value(envelope).unwrap();
            assert_eq!(
                serde_json::to_value(forwarded.try_recv().unwrap()).unwrap(),
                expected
            );
            assert_eq!(
                serde_json::to_value(subscribed.try_recv().unwrap()).unwrap(),
                expected
            );
        }
        assert!(forwarded.try_recv().is_err());
        assert!(subscribed.try_recv().is_err());
    }

    #[test]
    fn legacy_llm_call_without_context_plan_id_still_deserializes() {
        let json = r#"{"type":"llm_call","model":"m","provider":"p","usage":{"input":1,"cached_input":0,"output":0,"cache_write":0,"reasoning_tokens":0},"wallclock_ms":1,"ttft_ms":null,"tokens_per_second":null,"status":{"kind":"ok"},"run_id":null,"node_id":null}"#;
        let event: Event = serde_json::from_str(json).unwrap();

        assert!(matches!(
            event,
            Event::LlmCall {
                context_plan_id: None,
                context_epoch: None,
                context_tokens: None,
                usage_source: None,
                context_call_purpose: None,
                context_call_identity: None,
                context_cache: None,
                assistant_tool_batch_width: None,
                ..
            }
        ));
    }

    #[test]
    fn legacy_system_message_without_flow_owner_still_deserializes() {
        let turn_id = TurnId::now();
        let message = crate::message::Message::context_record(
            turn_id.clone(),
            crate::context_plan::ContextRecord::new(
                "session.goal",
                1,
                crate::context_plan::ContextRecordAuthority::User,
                crate::context_plan::ContextRecordRetention::Latest,
                crate::context_plan::ContextRecordBody::text("ship"),
            ),
        );
        let event: Event = serde_json::from_value(serde_json::json!({
            "type": "system_msg",
            "turn_id": turn_id,
            "message": message,
        }))
        .unwrap();

        assert!(matches!(
            event,
            Event::SystemMsg {
                flow_run_id: None,
                ..
            }
        ));
    }

    #[test]
    fn legacy_compaction_events_without_flow_owner_still_deserialize() {
        for value in [
            serde_json::json!({
                "type": "compaction_summary",
                "session_id": "session",
                "range_start": 0,
                "range_end": 1,
                "compacted_count": 2,
                "before_tokens": 100,
                "after_tokens": 10,
                "summary": "summary",
            }),
            serde_json::json!({
                "type": "context_compact",
                "session_id": "session",
                "before_tokens": 100,
                "after_tokens": 10,
                "compacted_range_start": 0,
                "compacted_range_end": 1,
            }),
            serde_json::json!({
                "type": "checkpoint",
                "session_id": "session",
                "messages": [],
                "window_tokens": 10,
            }),
        ] {
            let event: Event = serde_json::from_value(value).unwrap();
            assert!(match event {
                Event::CompactionSummary { flow_run_id, .. }
                | Event::ContextCompact { flow_run_id, .. }
                | Event::Checkpoint { flow_run_id, .. } => flow_run_id.is_none(),
                _ => false,
            });
        }
    }
}
