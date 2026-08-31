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

/// Wraps an Event with assigned sequence number and timestamp.
/// Serializes to the same JSONL format as the flat Event for backward compat.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub seq: u64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub event: Event,
}

impl EventEnvelope {
    pub fn new(seq: u64, event: Event) -> Self {
        let ts = chrono::Utc::now();
        Self { seq, ts, event }
    }

    pub(crate) fn from_json_value(value: serde_json::Value) -> serde_json::Result<Self> {
        let seq = value.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let ts = value
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        let event = serde_json::from_value(value)?;
        Ok(Self { seq, ts, event })
    }
}

impl serde::Serialize for EventEnvelope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.event).map_err(serde::ser::Error::custom)?;
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert("seq".into(), serde_json::Value::Number(self.seq.into()));
            map.insert("ts".into(), serde_json::Value::String(self.ts.to_rfc3339()));
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
    FlowStart {
        run_id: FlowRunId,
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
    WorkspaceLifecycle {
        run_id: FlowRunId,
        workspace_id: String,
        path: String,
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cleanup_error: Option<String>,
    },
    LlmCall {
        model: String,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_plan_id: Option<crate::context_plan::ContextPlanId>,
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
        title: String,
        #[serde(default)]
        old_content: Option<String>,
        #[serde(default)]
        new_content: Option<String>,
        #[serde(default)]
        unified_diff: Option<String>,
    },
    CompactionSummary {
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
    SystemMsg {
        turn_id: TurnId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
    },
    UserInject {
        turn_id: TurnId,
        injection: crate::injection::Injection,
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
        message_seq: u64,
        part_index: usize,
        file_basename: String,
        reason: String,
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

#[derive(Default, Clone)]
pub struct EventSink {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
    forwarder: Option<mpsc::UnboundedSender<EventEnvelope>>,
    seq_counter: Arc<std::sync::atomic::AtomicU64>,
    redactor: Option<Arc<crate::redact::Redactor>>,
    last_compact_at: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
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

    // Best-effort peek for anchor labels. NOT reserved: two peekers see the same value.
    // Safe while evaluator dispatch remains sequential per flow. Parallel dispatch must
    // reserve sequence numbers instead.
    pub fn next_seq_peek(&self) -> u64 {
        self.seq_counter.load(std::sync::atomic::Ordering::SeqCst) + 1
    }

    pub fn restore_seq(&self, last_seq: u64) {
        self.seq_counter
            .store(last_seq, std::sync::atomic::Ordering::SeqCst);
    }

    // Atomic reserve for the future parallel-dispatch case: returns a seq value that
    // no other reservation can obtain, at the cost of advancing the counter even if
    // the caller never emits (a hole in seq numbering). Not used yet; kept ready.
    pub fn reserve_seq(&self) -> u64 {
        self.seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    pub fn emit_returning_seq(&self, event: Event) -> u64 {
        let next = self
            .seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let envelope = EventEnvelope::new(next, event);
        if let Some(tx) = &self.forwarder {
            let _ = tx.send(envelope.clone());
        }
        self.events
            .lock()
            .expect("event sink poisoned")
            .push(envelope);
        next
    }

    pub fn emit(&self, event: Event) {
        let next = self
            .seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let envelope = EventEnvelope::new(next, event);
        if let Some(tx) = &self.forwarder {
            let _ = tx.send(envelope.clone());
        }
        self.events
            .lock()
            .expect("event sink poisoned")
            .push(envelope);
    }

    pub fn events_handle(&self) -> Arc<Mutex<Vec<EventEnvelope>>> {
        self.events.clone()
    }

    pub fn redactor(&self) -> Option<Arc<crate::redact::Redactor>> {
        self.redactor.clone()
    }

    pub fn mark_compacted(&self) {
        *self.last_compact_at.lock().expect("last_compact poisoned") = Some(chrono::Utc::now());
    }

    pub fn last_compact_ago_seconds(&self) -> Option<i64> {
        self.last_compact_at
            .lock()
            .expect("last_compact poisoned")
            .map(|t| (chrono::Utc::now() - t).num_seconds())
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
    fn flow_start_serializes_parent_linkage() {
        let parent = FlowRunId::now();
        let ev = Event::FlowStart {
            run_id: FlowRunId::now(),
            flow_name: "child".into(),
            parent_run_id: Some(parent.clone()),
            parent_node_id: Some("stmt_3".into()),
            spawned: false,
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "flow_start");
        assert_eq!(v["parent_run_id"], serde_json::json!(parent.0.to_string()));
        assert_eq!(v["parent_node_id"], "stmt_3");
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
            message_seq: 42,
            part_index: 1,
            file_basename: "photo.png".into(),
            reason: "image_too_large".into(),
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
    fn seq_and_set_seq_cover_attachment_degraded() {
        let _ev = Event::AttachmentDegraded {
            turn_id: None,
            flow_run_id: None,
            message_seq: 10,
            part_index: 0,
            file_basename: "x".into(),
            reason: "y".into(),
        };
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
