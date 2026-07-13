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

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    FlowStart {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        flow_name: String,
        parent_run_id: Option<FlowRunId>,
        parent_node_id: Option<String>,
        ts: chrono::DateTime<chrono::Utc>,
    },
    FlowEnd {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        flow_name: String,
        status: FlowStatus,
        ts: chrono::DateTime<chrono::Utc>,
    },
    LlmCall {
        #[serde(default)]
        seq: u64,
        model: String,
        provider: String,
        usage: crate::provider::TokenUsage,
        wallclock_ms: u64,
        ttft_ms: Option<u64>,
        tokens_per_second: Option<f64>,
        status: LlmCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<crate::event::FlowRunId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node_id: Option<String>,
        ts: chrono::DateTime<chrono::Utc>,
    },
    TurnStart {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        ts: chrono::DateTime<chrono::Utc>,
    },
    TurnEnd {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        ts: chrono::DateTime<chrono::Utc>,
    },
    UserMsg {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    AssistantMsg {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ToolResultMsg {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        flow_run_id: Option<FlowRunId>,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    SystemMsg {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        message: crate::message::Message,
        ts: chrono::DateTime<chrono::Utc>,
    },
    UserInject {
        #[serde(default)]
        seq: u64,
        turn_id: TurnId,
        injection: crate::injection::Injection,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ContentFilterHit {
        #[serde(default)]
        seq: u64,
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        provider: String,
        model: String,
        category: String,
        action: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ContextCompact {
        #[serde(default)]
        seq: u64,
        session_id: String,
        before_tokens: u64,
        after_tokens: u64,
        compacted_range_start: u64,
        compacted_range_end: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_msg_seq: Option<u64>,
        ts: chrono::DateTime<chrono::Utc>,
    },
    Checkpoint {
        #[serde(default)]
        seq: u64,
        session_id: String,
        messages: Vec<crate::message::Message>,
        window_tokens: u64,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ContextTruncated {
        #[serde(default)]
        seq: u64,
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        original_chars: u64,
        result_chars: u64,
        dropped_chars: u64,
        budget_tokens: u64,
        ts: chrono::DateTime<chrono::Utc>,
    },
    WatchWarn {
        #[serde(default)]
        seq: u64,
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        target: String,
        trigger: String,
        message: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    LlmPartialCall {
        #[serde(default)]
        seq: u64,
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        model: String,
        provider: String,
        tokens_before_abort: u64,
        restart_reason: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    PendingPrompt {
        #[serde(default)]
        seq: u64,
        prompt_id: uuid::Uuid,
        kind: String,
        payload: serde_json::Value,
        ts: chrono::DateTime<chrono::Utc>,
    },
    PromptResolved {
        #[serde(default)]
        seq: u64,
        prompt_id: uuid::Uuid,
        answer: serde_json::Value,
        ts: chrono::DateTime<chrono::Utc>,
    },
    FlowGraph {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        graph: crate::nodegraph::FlowGraph,
        ts: chrono::DateTime<chrono::Utc>,
    },
    FlowNodeStart {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        node_id: String,
        kind: crate::nodegraph::NodeKind,
        label: String,
        parent_node_id: Option<String>,
        ts: chrono::DateTime<chrono::Utc>,
    },
    FlowNodeEnd {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        node_id: String,
        status: FlowNodeStatus,
        output_preview: Option<String>,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ToolNode {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        parent_node_id: String,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    AttachmentDegraded {
        #[serde(default)]
        seq: u64,
        turn_id: Option<TurnId>,
        flow_run_id: Option<FlowRunId>,
        message_seq: u64,
        part_index: usize,
        file_basename: String,
        reason: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ToolPendingApproval {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        level: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ToolApproved {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        tool_use_id: String,
        decided_by: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
    ToolDenied {
        #[serde(default)]
        seq: u64,
        run_id: FlowRunId,
        tool_use_id: String,
        reason: String,
        ts: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FlowNodeStatus {
    Ok,
    Err,
    Cancelled,
}

impl Event {
    pub fn set_seq(&mut self, new_seq: u64) {
        match self {
            Event::FlowStart { seq, .. }
            | Event::FlowEnd { seq, .. }
            | Event::LlmCall { seq, .. }
            | Event::TurnStart { seq, .. }
            | Event::TurnEnd { seq, .. }
            | Event::UserMsg { seq, .. }
            | Event::AssistantMsg { seq, .. }
            | Event::ToolResultMsg { seq, .. }
            | Event::SystemMsg { seq, .. }
            | Event::UserInject { seq, .. }
            | Event::ContentFilterHit { seq, .. }
            | Event::ContextCompact { seq, .. }
            | Event::Checkpoint { seq, .. }
            | Event::ContextTruncated { seq, .. }
            | Event::WatchWarn { seq, .. }
            | Event::PendingPrompt { seq, .. }
            | Event::PromptResolved { seq, .. }
            | Event::LlmPartialCall { seq, .. }
            | Event::FlowGraph { seq, .. }
            | Event::FlowNodeStart { seq, .. }
            | Event::FlowNodeEnd { seq, .. }
            | Event::ToolNode { seq, .. }
            | Event::AttachmentDegraded { seq, .. }
            | Event::ToolPendingApproval { seq, .. }
            | Event::ToolApproved { seq, .. }
            | Event::ToolDenied { seq, .. } => *seq = new_seq,
        }
    }

    pub fn seq(&self) -> u64 {
        match self {
            Event::FlowStart { seq, .. }
            | Event::FlowEnd { seq, .. }
            | Event::LlmCall { seq, .. }
            | Event::TurnStart { seq, .. }
            | Event::TurnEnd { seq, .. }
            | Event::UserMsg { seq, .. }
            | Event::AssistantMsg { seq, .. }
            | Event::ToolResultMsg { seq, .. }
            | Event::SystemMsg { seq, .. }
            | Event::UserInject { seq, .. }
            | Event::ContentFilterHit { seq, .. }
            | Event::ContextCompact { seq, .. }
            | Event::Checkpoint { seq, .. }
            | Event::ContextTruncated { seq, .. }
            | Event::WatchWarn { seq, .. }
            | Event::PendingPrompt { seq, .. }
            | Event::PromptResolved { seq, .. }
            | Event::LlmPartialCall { seq, .. }
            | Event::FlowGraph { seq, .. }
            | Event::FlowNodeStart { seq, .. }
            | Event::FlowNodeEnd { seq, .. }
            | Event::ToolNode { seq, .. }
            | Event::AttachmentDegraded { seq, .. }
            | Event::ToolPendingApproval { seq, .. }
            | Event::ToolApproved { seq, .. }
            | Event::ToolDenied { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowStatus {
    Ok,
    Errored { message: String },
}

impl FlowStatus {
    pub fn errored(msg: impl Into<String>) -> Self {
        Self::Errored {
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmCallStatus {
    Ok,
    Errored { message: String },
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
        seq: u64,
    },
    ToolStderrLine {
        line: String,
        seq: u64,
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
    events: Arc<Mutex<Vec<Event>>>,
    forwarder: Option<mpsc::UnboundedSender<Event>>,
    seq_counter: Arc<std::sync::atomic::AtomicU64>,
    redactor: Option<Arc<crate::redact::Redactor>>,
    last_compact_at: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_forwarder(mut self, tx: mpsc::UnboundedSender<Event>) -> Self {
        self.forwarder = Some(tx);
        self
    }

    pub fn with_redactor(mut self, redactor: Arc<crate::redact::Redactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    // Best-effort peek for anchor labels. NOT reserved: two peekers see the same value.
    // Safe today because eval is single-threaded per flow (see tool.rs comment on !Send).
    // If parallel-tool dispatch is introduced, switch call sites to reserve_seq.
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
        let mut event = event;
        event.set_seq(next);
        if let Some(tx) = &self.forwarder {
            let _ = tx.send(event.clone());
        }
        self.events.lock().expect("event sink poisoned").push(event);
        next
    }

    pub fn emit(&self, mut event: Event) {
        let next = self
            .seq_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        event.set_seq(next);
        if let Some(tx) = &self.forwarder {
            let _ = tx.send(event.clone());
        }
        self.events.lock().expect("event sink poisoned").push(event);
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
    }

    pub fn snapshot(&self) -> Vec<Event> {
        self.events.lock().expect("event sink poisoned").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_start_serializes_parent_linkage() {
        let parent = FlowRunId::now();
        let ev = Event::FlowStart {
            seq: 42,
            run_id: FlowRunId::now(),
            flow_name: "child".into(),
            parent_run_id: Some(parent.clone()),
            parent_node_id: Some("stmt_3".into()),
            ts: chrono::Utc::now(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "flow_start");
        assert_eq!(v["parent_run_id"], serde_json::json!(parent.0.to_string()));
        assert_eq!(v["parent_node_id"], "stmt_3");
    }

    #[test]
    fn flow_node_start_carries_parent_node_id() {
        let ev = Event::FlowNodeStart {
            seq: 7,
            run_id: FlowRunId::now(),
            node_id: "stmt_1.branch[0]".into(),
            kind: crate::nodegraph::NodeKind::UserConfirm,
            label: "fanout".into(),
            parent_node_id: Some("stmt_1".into()),
            ts: chrono::Utc::now(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "flow_node_start");
        assert_eq!(v["parent_node_id"], "stmt_1");
    }

    #[test]
    fn tool_node_serializes_all_fields() {
        let run_id = FlowRunId::now();
        let ev = Event::ToolNode {
            seq: 12,
            run_id: run_id.clone(),
            parent_node_id: "stmt_2".into(),
            tool_use_id: "tu_abc".into(),
            tool_name: "fs.read".into(),
            args_preview: "{\"path\":\"a.rs\"}".into(),
            ts: chrono::Utc::now(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "tool_node");
        assert_eq!(v["run_id"], run_id.0.to_string());
        assert_eq!(v["parent_node_id"], "stmt_2");
        assert_eq!(v["tool_use_id"], "tu_abc");
        assert_eq!(v["tool_name"], "fs.read");
        assert_eq!(v["args_preview"], "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn seq_and_set_seq_cover_tool_node() {
        let mut ev = Event::ToolNode {
            seq: 0,
            run_id: FlowRunId::now(),
            parent_node_id: "s".into(),
            tool_use_id: "t".into(),
            tool_name: "n".into(),
            args_preview: "{}".into(),
            ts: chrono::Utc::now(),
        };
        ev.set_seq(99);
        assert_eq!(ev.seq(), 99);
    }

    #[test]
    fn attachment_degraded_serializes_all_fields() {
        let turn = TurnId::now();
        let flow = FlowRunId::now();
        let ev = Event::AttachmentDegraded {
            seq: 55,
            turn_id: Some(turn.clone()),
            flow_run_id: Some(flow.clone()),
            message_seq: 42,
            part_index: 1,
            file_basename: "photo.png".into(),
            reason: "image_too_large".into(),
            ts: chrono::Utc::now(),
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
        let mut ev = Event::AttachmentDegraded {
            seq: 0,
            turn_id: None,
            flow_run_id: None,
            message_seq: 10,
            part_index: 0,
            file_basename: "x".into(),
            reason: "y".into(),
            ts: chrono::Utc::now(),
        };
        ev.set_seq(101);
        assert_eq!(ev.seq(), 101);
    }

    #[test]
    fn tool_pending_approval_round_trip() {
        let ev = Event::ToolPendingApproval {
            seq: 5,
            run_id: FlowRunId::now(),
            tool_use_id: "tu1".into(),
            tool_name: "fs.write".into(),
            args_preview: "{}".into(),
            level: "approve".into(),
            preview: None,
            ts: chrono::Utc::now(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "tool_pending_approval");
        assert_eq!(v["tool_use_id"], "tu1");
        assert_eq!(v["level"], "approve");
    }

    #[test]
    fn seq_and_set_seq_cover_approval_variants() {
        let rid = FlowRunId::now();
        for mut ev in [
            Event::ToolPendingApproval {
                seq: 0,
                run_id: rid.clone(),
                tool_use_id: "t".into(),
                tool_name: "n".into(),
                args_preview: "{}".into(),
                level: "approve".into(),
                preview: None,
                ts: chrono::Utc::now(),
            },
            Event::ToolApproved {
                seq: 0,
                run_id: rid.clone(),
                tool_use_id: "t".into(),
                decided_by: "user".into(),
                ts: chrono::Utc::now(),
            },
            Event::ToolDenied {
                seq: 0,
                run_id: rid.clone(),
                tool_use_id: "t".into(),
                reason: "no".into(),
                ts: chrono::Utc::now(),
            },
        ] {
            ev.set_seq(77);
            assert_eq!(ev.seq(), 77);
        }
    }
}
