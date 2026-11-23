//! Bounded browser display projection over a persistent Flow journal.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    flows::FlowAddress,
    history::{HistoryHead, HistorySource},
    responses::InputTurn,
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
    /// Requests original message-part ordinals for use with the flow.drafts capability.
    #[serde(default)]
    pub include_source_parts: bool,
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
    /// Emitted only on request. Synthetic authored rows have null ordinals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_parts: Option<Vec<Option<usize>>>,
    /// Verified authored admission for a User row; absent for inherited or host input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_turn: Option<crate::history::ConversationCutoff>,
    /// Emitted with source_parts when the selected record has a native turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<ConversationWorkMember>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ConversationWorkMember {
    pub lease: InputTurn,
    pub role: ConversationWorkRole,
    pub publication: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConversationWorkRole {
    Invocation,
    Work,
    FinalAnswer,
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
    Text {
        text: String,
    },
    Quote {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        name: String,
        /// Opaque correlation with the loaded Flow's User approval queue.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_key: Option<String>,
    },
    ToolResult {
        failed: bool,
        /// Bounded text from the committed message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(max_length = 2048)]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        preview_truncated: bool,
    },
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
