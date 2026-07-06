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
    FlowNodeStart {
        run_id: String,
        node_id: String,
        kind: crate::nodegraph::NodeKind,
        label: String,
    },
    FlowNodeEnd {
        run_id: String,
        node_id: String,
        status: crate::event::FlowNodeStatus,
        output_preview: Option<String>,
    },
    FlowDone {
        run_id: String,
        flow_name: String,
        ok: bool,
    },
}
