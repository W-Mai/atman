//! Read-only browser snapshots from an existing Flow execution owner.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    conversation::ConversationItemId,
    flows::{FlowAddress, FlowCursor},
    history::HistoryHead,
    responses::InputTurn,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFlowDraftsRequest {
    pub flow: FlowAddress,
    pub head: HistoryHead,
    /// A previously observed edition. A matching edition waits for the owner's next change.
    pub known: Option<FlowDraftEdition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowDraftEdition {
    pub host_generation: Uuid,
    pub worker_incarnation: Uuid,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReadFlowDraftsResponse {
    Ready {
        snapshot: Box<FlowDraftSnapshot>,
    },
    Unchanged {
        flow: FlowAddress,
        head: HistoryHead,
        known: FlowDraftEdition,
    },
    HistoryChanged {
        flow: FlowAddress,
        head: HistoryHead,
    },
    Unavailable {
        flow: FlowAddress,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftSnapshot {
    pub flow: FlowAddress,
    pub head: HistoryHead,
    pub host_generation: Uuid,
    pub worker_incarnation: Uuid,
    pub after: FlowCursor,
    pub revision: u64,
    pub active: Option<InputTurn>,
    pub work: Option<FlowDraftWork>,
    pub calls: Vec<FlowDraftCall>,
    pub final_answer: Option<FlowDraftCall>,
    pub handoffs: Vec<FlowDraftHandoff>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftWork {
    pub lease: InputTurn,
    pub admission: Option<FlowCursor>,
    pub latest_final: Option<FlowFinalBoundary>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowFinalBoundary {
    pub publication: Uuid,
    pub receipt: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftCall {
    pub id: Uuid,
    pub lease: InputTurn,
    pub run: Uuid,
    pub attempt: u64,
    pub revision: u64,
    pub publication_pending: bool,
    pub parts: Vec<FlowDraftPart>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftId {
    pub call: Uuid,
    pub attempt: u64,
    pub part: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftPart {
    pub id: FlowDraftId,
    pub body: FlowDraftPartBody,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowDraftPartBody {
    Text {
        value: DraftText,
    },
    Thinking {
        value: DraftText,
    },
    FinalAnswer {
        value: DraftText,
    },
    Tool {
        index: usize,
        call_id: DraftText,
        name: DraftText,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct DraftText {
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftHandoff {
    pub lease: InputTurn,
    pub run: Uuid,
    pub publication: Uuid,
    pub receipt: FlowCursor,
    pub item: ConversationItemId,
    pub parts: Vec<FlowDraftHandoffPart>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowDraftHandoffPart {
    pub live: FlowDraftId,
    pub published_parts: Vec<usize>,
}
