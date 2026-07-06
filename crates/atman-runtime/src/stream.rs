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
}
