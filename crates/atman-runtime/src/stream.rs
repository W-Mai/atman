use crate::notify::{NotifyLevel, NotifyLifecycle, NotifyLocation, NotifyStack};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationFrame {
    pub level: NotifyLevel,
    pub location: NotifyLocation,
    pub lifecycle: NotifyLifecycle,
    pub stack: NotifyStack,
    pub message: String,
}

impl From<crate::notify::Notification> for NotificationFrame {
    fn from(n: crate::notify::Notification) -> Self {
        Self {
            level: n.level,
            location: n.location,
            lifecycle: n.lifecycle,
            stack: n.stack,
            message: n.message,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    Running,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    TurnStarted {
        turn_id: String,
    },
    TurnEnded {
        turn_id: String,
    },
    LlmChunk {
        text: String,
        model: String,
        #[serde(default)]
        run_id: Option<String>,
    },
    ThinkingChunk {
        text: String,
        #[serde(default)]
        run_id: Option<String>,
    },
    ToolCallDraft {
        index: usize,
        call_id: String,
        name: String,
        arguments_delta: String,
        #[serde(default)]
        run_id: Option<String>,
    },
    LlmDone {
        total_tokens: u64,
        #[serde(default)]
        run_id: Option<String>,
    },
    /// Discard streaming output from the previous attempt before retrying.
    LlmRetry,
    LlmCallStats {
        model: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        context_call_purpose: crate::context_plan::ContextCallPurpose,
        #[serde(default)]
        context_call_scope: crate::context_plan::ContextCallScope,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_write: u64,
        ttft_ms: u64,
        tokens_per_second: f64,
        wallclock_ms: u64,
        run_id: Option<String>,
        node_id: Option<String>,
    },
    ToolUseStart {
        tool: String,
        args_preview: String,
        id: String,
    },
    ToolUseDone {
        tool: String,
        ok: bool,
        preview: String,
        id: String,
    },
    Note(String),
    /// Rich notification with level/location/lifecycle/stack.
    Notification(NotificationFrame),
    FlowGraph {
        run_id: String,
        graph: crate::nodegraph::FlowGraph,
    },
    FlowStart {
        run_id: String,
        flow_name: String,
        #[serde(default)]
        parent_run_id: Option<String>,
        #[serde(default)]
        parent_node_id: Option<String>,
    },
    FlowNodeStart {
        run_id: String,
        node_id: String,
        kind: crate::nodegraph::NodeKind,
        label: String,
        #[serde(default)]
        parent_node_id: Option<String>,
    },
    FlowNodeEnd {
        run_id: String,
        node_id: String,
        status: crate::event::FlowNodeStatus,
        output_preview: Option<String>,
        #[serde(default)]
        parent_node_id: Option<String>,
    },
    FlowDone {
        run_id: String,
        flow_name: String,
        ok: bool,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        suicide: bool,
    },
    ToolNode {
        run_id: String,
        parent_node_id: String,
        tool_use_id: String,
        tool: String,
        args_preview: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
    },
    AssistantMsg {
        flow_run_id: Option<String>,
        message: crate::message::Message,
    },
    ToolResultMsg {
        flow_run_id: Option<String>,
        message: crate::message::Message,
    },
    ToolPendingApproval {
        run_id: String,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        level: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    ToolApproved {
        run_id: String,
        tool_use_id: String,
        decided_by: String,
    },
    ToolDenied {
        run_id: String,
        tool_use_id: String,
        reason: String,
    },
    PermissionRequestCreated {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestTargeted {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestDeferred {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestApproved {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestDenied {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionRequestCancelled {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    PermissionGroupCreated {
        run_id: String,
        payload: crate::permission_audit::PermissionGroupAudit,
    },
    PermissionGroupUpdated {
        run_id: String,
        payload: crate::permission_audit::PermissionGroupAudit,
    },
    PermissionGroupResolved {
        run_id: String,
        payload: crate::permission_audit::PermissionGroupAudit,
    },
    PermissionGrantCreated {
        run_id: String,
        payload: crate::permission_audit::PermissionGrantAudit,
    },
    PermissionGrantExpired {
        run_id: String,
        payload: crate::permission_audit::PermissionGrantAudit,
    },
    UnrestrictedExecution {
        run_id: String,
        payload: crate::permission_audit::PermissionRequestAudit,
    },
    TerminalChunk {
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        bytes: Vec<u8>,
        screen: Option<crate::tools::term::TerminalScreen>,
        state: crate::tools::term::TermStateSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
        #[serde(default)]
        run_id: Option<String>,
    },
    TerminalExited {
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
        #[serde(default)]
        run_id: Option<String>,
    },
    BashChunk {
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        kind: String,
        line: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
        #[serde(default)]
        run_id: Option<String>,
    },
    BashExited {
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        exit_code: Option<i32>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
        #[serde(default)]
        run_id: Option<String>,
    },
    DiffPreview {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
        #[serde(default)]
        run_id: Option<String>,
    },
    FileEditApplied {
        #[serde(default)]
        turn_id: Option<String>,
        #[serde(default)]
        run_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        tool_name: String,
        path: String,
        metrics: crate::activity::EditMetrics,
    },
    CompactionSummary {
        phase: CompactionPhase,
        range_start: usize,
        range_end: usize,
        summary: String,
        before_tokens: u64,
        after_tokens: u64,
        compacted_count: usize,
    },
    CompactionDelta {
        range_start: usize,
        range_end: usize,
        text: String,
    },
    MermaidDiagram {
        source: String,
    },
    SubAgentStarted {
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        goal: String,
        child_run_id: String,
        model: String,
    },
    SubAgentDone {
        handle: String,
        status: String,
        final_text: String,
    },
    #[serde(other)]
    Unknown,
}

/// Extract the run_id (or flow_run_id) from any StreamFrame variant that carries one.
/// Used to route frames to the correct sub-agent's entry / TUI item.
pub fn frame_run_id(frame: &StreamFrame) -> Option<&str> {
    match frame {
        StreamFrame::FlowStart { run_id, .. }
        | StreamFrame::FlowNodeStart { run_id, .. }
        | StreamFrame::FlowNodeEnd { run_id, .. }
        | StreamFrame::FlowDone { run_id, .. }
        | StreamFrame::FlowGraph { run_id, .. }
        | StreamFrame::ToolNode { run_id, .. }
        | StreamFrame::ToolPendingApproval { run_id, .. }
        | StreamFrame::ToolApproved { run_id, .. }
        | StreamFrame::ToolDenied { run_id, .. }
        | StreamFrame::PermissionRequestCreated { run_id, .. }
        | StreamFrame::PermissionRequestTargeted { run_id, .. }
        | StreamFrame::PermissionRequestDeferred { run_id, .. }
        | StreamFrame::PermissionRequestApproved { run_id, .. }
        | StreamFrame::PermissionRequestDenied { run_id, .. }
        | StreamFrame::PermissionRequestCancelled { run_id, .. }
        | StreamFrame::PermissionGroupCreated { run_id, .. }
        | StreamFrame::PermissionGroupUpdated { run_id, .. }
        | StreamFrame::PermissionGroupResolved { run_id, .. }
        | StreamFrame::PermissionGrantCreated { run_id, .. }
        | StreamFrame::PermissionGrantExpired { run_id, .. }
        | StreamFrame::UnrestrictedExecution { run_id, .. } => Some(run_id.as_str()),
        StreamFrame::AssistantMsg {
            flow_run_id: Some(rid),
            ..
        }
        | StreamFrame::ToolResultMsg {
            flow_run_id: Some(rid),
            ..
        }
        | StreamFrame::LlmCallStats {
            run_id: Some(rid), ..
        } => Some(rid.as_str()),
        StreamFrame::LlmChunk {
            run_id: Some(rid), ..
        }
        | StreamFrame::ThinkingChunk {
            run_id: Some(rid), ..
        }
        | StreamFrame::ToolCallDraft {
            run_id: Some(rid), ..
        }
        | StreamFrame::LlmDone {
            run_id: Some(rid), ..
        }
        | StreamFrame::TerminalChunk {
            run_id: Some(rid), ..
        }
        | StreamFrame::TerminalExited {
            run_id: Some(rid), ..
        }
        | StreamFrame::BashChunk {
            run_id: Some(rid), ..
        }
        | StreamFrame::BashExited {
            run_id: Some(rid), ..
        }
        | StreamFrame::DiffPreview {
            run_id: Some(rid), ..
        }
        | StreamFrame::FileEditApplied {
            run_id: Some(rid), ..
        } => Some(rid.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_node_round_trips() {
        let f = StreamFrame::ToolNode {
            run_id: "r".into(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_1".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamFrame::ToolNode { .. }));
    }

    #[test]
    fn legacy_llm_stats_default_route_metadata() {
        let json = r#"{"LlmCallStats":{"model":"m","input_tokens":1,"output_tokens":2,"cache_read":3,"cache_write":4,"ttft_ms":5,"tokens_per_second":6.0,"wallclock_ms":7,"run_id":null,"node_id":null}}"#;
        let frame: StreamFrame = serde_json::from_str(json).unwrap();

        assert!(matches!(
            frame,
            StreamFrame::LlmCallStats {
                provider,
                context_call_purpose: crate::context_plan::ContextCallPurpose::General,
                context_call_scope: crate::context_plan::ContextCallScope::Detached,
                ..
            } if provider.is_empty()
        ));
    }

    #[test]
    fn flow_node_start_serde_carries_parent() {
        let f = StreamFrame::FlowNodeStart {
            run_id: "r".into(),
            node_id: "stmt_1.branch[0]".into(),
            kind: crate::nodegraph::NodeKind::UserConfirm,
            label: "b".into(),
            parent_node_id: Some("stmt_1".into()),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"parent_node_id\":\"stmt_1\""));
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        if let StreamFrame::FlowNodeStart { parent_node_id, .. } = back {
            assert_eq!(parent_node_id.as_deref(), Some("stmt_1"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn unknown_bare_variant_falls_back() {
        let payload = r#""SomeFutureFrame""#;
        let back: StreamFrame = serde_json::from_str(payload).unwrap();
        assert!(matches!(back, StreamFrame::Unknown));
    }

    #[test]
    fn terminal_chunk_round_trips() {
        let screen = crate::tools::term::TerminalScreen {
            rows: 2,
            cols: 3,
            cells: vec![
                crate::tools::term::TerminalCell {
                    chars: "A".into(),
                    ..Default::default()
                },
                crate::tools::term::TerminalCell::default(),
                crate::tools::term::TerminalCell::default(),
                crate::tools::term::TerminalCell::default(),
                crate::tools::term::TerminalCell::default(),
                crate::tools::term::TerminalCell::default(),
            ],
            cursor: Some((0, 0)),
            alt_screen: false,
        };
        let f = StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen),
            state: crate::tools::term::TermStateSnapshot::Running,
            call_intent: crate::message::ToolCallIntent::new("Inspect terminal output"),
            tool_use_id: None,
            run_id: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        match back {
            StreamFrame::TerminalChunk {
                handle,
                bytes,
                screen,
                state,
                call_intent,
                run_id,
                ..
            } => {
                assert_eq!(handle, "term_s_0");
                assert_eq!(bytes, b"hi");
                assert!(run_id.is_none());
                assert_eq!(
                    call_intent.as_ref().map(|intent| intent.as_str()),
                    Some("Inspect terminal output")
                );
                let screen = screen.expect("screen should be Some");
                assert_eq!(screen.rows, 2);
                assert_eq!(screen.cols, 3);
                assert_eq!(screen.cells.len(), 6);
                assert_eq!(screen.cells[0].chars, "A");
                assert!(matches!(
                    state,
                    crate::tools::term::TermStateSnapshot::Running
                ));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn bash_exited_error_round_trips_and_legacy_payload_loads() {
        let frame = StreamFrame::BashExited {
            handle: "bg_s_1".into(),
            exit_code: None,
            error: Some("open log: permission denied".into()),
            call_intent: crate::message::ToolCallIntent::new("Run verification"),
            tool_use_id: None,
            run_id: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        match back {
            StreamFrame::BashExited {
                error, call_intent, ..
            } => {
                assert_eq!(error.as_deref(), Some("open log: permission denied"));
                assert_eq!(
                    call_intent.as_ref().map(|intent| intent.as_str()),
                    Some("Run verification")
                );
            }
            _ => panic!("wrong variant"),
        }

        let legacy = r#"{"BashExited":{"handle":"bg_s_1","exit_code":null,"run_id":null}}"#;
        let back: StreamFrame = serde_json::from_str(legacy).unwrap();
        match back {
            StreamFrame::BashExited {
                error, call_intent, ..
            } => {
                assert!(error.is_none());
                assert!(call_intent.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn terminal_exited_round_trips() {
        let f = StreamFrame::TerminalExited {
            handle: "term_s_1".into(),
            exit_code: Some(0),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        match back {
            StreamFrame::TerminalExited {
                handle, exit_code, ..
            } => {
                assert_eq!(handle, "term_s_1");
                assert_eq!(exit_code, Some(0));
            }
            _ => panic!("wrong variant"),
        }

        let legacy = r#"{"TerminalExited":{"handle":"term_s_1","exit_code":0,"run_id":null}}"#;
        let back: StreamFrame = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            back,
            StreamFrame::TerminalExited {
                call_intent: None,
                ..
            }
        ));
    }

    #[test]
    fn compaction_summary_round_trips() {
        let f = StreamFrame::CompactionSummary {
            phase: CompactionPhase::Running,
            range_start: 3,
            range_end: 11,
            summary: String::new(),
            before_tokens: 42,
            after_tokens: 0,
            compacted_count: 8,
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        match back {
            StreamFrame::CompactionSummary {
                phase,
                range_start,
                range_end,
                compacted_count,
                ..
            } => {
                assert_eq!(phase, CompactionPhase::Running);
                assert_eq!(range_start, 3);
                assert_eq!(range_end, 11);
                assert_eq!(compacted_count, 8);
            }
            _ => panic!("wrong variant"),
        }
    }
}
