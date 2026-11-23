//! Read-only byte checkpoints for background process output.

use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationLogRecord {
    pub commit: u64,
    pub fact: usize,
    pub item: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationLogSource {
    pub receipt: FlowCursor,
    pub record: ConversationLogRecord,
    pub lease: InputTurn,
    pub handle: String,
    /// A writer checkpoint supplies both fields. Historical tool results identify only the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drained: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationWorkLogs {
    #[schema(max_items = 64)]
    pub entries: Vec<ConversationLogSource>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationLogLocator {
    pub source: HistorySource,
    pub head: HistoryHead,
    pub receipt: FlowCursor,
    pub record: ConversationLogRecord,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadConversationLogRequest {
    pub locator: ConversationLogLocator,
    pub offset: usize,
    #[schema(minimum = 4, maximum = 32768)]
    pub limit: u32,
    pub verification: ArtifactVerification,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReadConversationLogResponse {
    Ready {
        locator: Box<ConversationLogLocator>,
        content: String,
        offset: usize,
        next_offset: usize,
        total_bytes: usize,
        has_more: bool,
    },
    Verifying {
        locator: Box<ConversationLogLocator>,
        verified_bytes: usize,
        total_bytes: usize,
    },
    Capacity {
        kind: ConversationCapacityKind,
        required_at_least: usize,
        allowed: usize,
    },
    VerificationBudget {
        required: usize,
        allowed: usize,
    },
    VerificationCancelled,
    SourceChanged,
    ContentChanged,
    InvalidRange,
    Unavailable,
}
