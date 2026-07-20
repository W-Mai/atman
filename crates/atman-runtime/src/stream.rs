use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    Running,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    LlmChunk {
        text: String,
        model: String,
    },
    ThinkingChunk {
        text: String,
    },
    LlmDone {
        total_tokens: u64,
    },
    LlmCallStats {
        model: String,
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
    },
    ToolNode {
        run_id: String,
        parent_node_id: String,
        tool_use_id: String,
        tool: String,
        args_preview: String,
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
    TerminalChunk {
        handle: String,
        bytes: Vec<u8>,
        screen: Option<crate::tools::term::TerminalScreen>,
        state: crate::tools::term::TermStateSnapshot,
    },
    TerminalExited {
        handle: String,
        exit_code: Option<i32>,
    },
    BashChunk {
        handle: String,
        kind: String,
        line: String,
    },
    BashExited {
        handle: String,
        exit_code: Option<i32>,
    },
    DiffPreview {
        title: String,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
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
    #[serde(other)]
    Unknown,
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
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamFrame::ToolNode { .. }));
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
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        match back {
            StreamFrame::TerminalChunk {
                handle,
                bytes,
                screen,
                state,
            } => {
                assert_eq!(handle, "term_s_0");
                assert_eq!(bytes, b"hi");
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
    fn terminal_exited_round_trips() {
        let f = StreamFrame::TerminalExited {
            handle: "term_s_1".into(),
            exit_code: Some(0),
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&json).unwrap();
        match back {
            StreamFrame::TerminalExited { handle, exit_code } => {
                assert_eq!(handle, "term_s_1");
                assert_eq!(exit_code, Some(0));
            }
            _ => panic!("wrong variant"),
        }
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
