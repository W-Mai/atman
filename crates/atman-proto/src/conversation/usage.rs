use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationUsageLocator {
    pub source: HistorySource,
    pub head: HistoryHead,
    /// A visible, committed conversation row owned by this invocation.
    pub item: ConversationItemId,
    pub lease: InputTurn,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadConversationUsageRequest {
    pub locator: ConversationUsageLocator,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationUsageSource {
    Provider,
    Mixed,
    Estimated,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationUsageRoute {
    #[schema(max_length = 512)]
    pub provider: String,
    #[schema(max_length = 512)]
    pub model: String,
    pub purpose: Option<String>,
    pub scope: Option<String>,
    pub source: ConversationUsageSource,
    pub calls: u32,
    pub failures: u32,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Calls with a positive recorded cache read. A missing lane can be stored as zero.
    pub cache_read_calls: u32,
    pub cache_write: u64,
    pub reasoning: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationUsageEdits {
    pub applied: usize,
    pub files: usize,
    pub hunks: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReadConversationUsageResponse {
    Ready {
        locator: Box<ConversationUsageLocator>,
        #[schema(max_items = 64)]
        routes: Vec<ConversationUsageRoute>,
        edits: ConversationUsageEdits,
        interrupted_calls: u32,
    },
    Capacity {
        kind: ConversationCapacityKind,
        required_at_least: usize,
        allowed: usize,
    },
    SourceChanged,
    Unavailable,
}
