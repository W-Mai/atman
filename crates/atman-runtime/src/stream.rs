use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    LlmChunk {
        text: String,
        model: String,
    },
    LlmDone {
        total_tokens: u64,
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
}
