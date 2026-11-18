//! Bounded browser display projection over a persistent Flow journal.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    flows::FlowAddress,
    history::{HistoryHead, HistorySource},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationItemId {
    pub commit: u64,
    pub fact: usize,
    pub item: usize,
    pub message: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationBefore {
    pub source: HistorySource,
    pub head: HistoryHead,
    pub item: ConversationItemId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadConversationRequest {
    pub flow: FlowAddress,
    pub before: Option<ConversationBefore>,
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ConversationPage {
    pub flow: FlowAddress,
    pub source: HistorySource,
    pub head: HistoryHead,
    pub trailing_bytes: u64,
    pub items: Vec<ConversationItem>,
    pub before: Option<ConversationBefore>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ConversationItem {
    pub id: ConversationItemId,
    pub body: ConversationBody,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationBody {
    Message {
        role: ConversationRole,
        turn: Uuid,
        parts: Vec<ConversationPart>,
    },
    Deferred {
        required_at_least: usize,
        allowed: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationPart {
    Text { text: String },
    Quote { text: String },
    Thinking { text: String },
    ToolCall { name: String },
    ToolResult { failed: bool },
    Image,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationCapacityKind {
    SourceRecord,
    DecodeWork,
    RetainedIndex,
    BranchMetadata,
    SelectionWork,
    PageSourceBytes,
    ScanBytes,
    ReadersBusy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReadConversationResponse {
    Ready {
        page: Box<ConversationPage>,
    },
    Capacity {
        flow: FlowAddress,
        kind: ConversationCapacityKind,
        required_at_least: usize,
        allowed: usize,
    },
}
