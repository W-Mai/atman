//! Pending operator interactions for an already loaded Flow.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    flows::{FlowAddress, FlowCursor},
    history::HistoryHead,
};

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFlowInteractionsRequest {
    pub flow: FlowAddress,
    pub head: HistoryHead,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadFlowInteractionsResponse {
    Ready {
        flow: FlowAddress,
        head: HistoryHead,
        host_generation: String,
        #[schema(max_items = 8)]
        forms: Vec<FlowForm>,
        #[serde(skip_serializing_if = "Option::is_none")]
        compact: Option<FlowCompactReview>,
        omitted: bool,
    },
    Unavailable {
        flow: FlowAddress,
        head: HistoryHead,
    },
    SourceChanged {
        flow: FlowAddress,
        head: HistoryHead,
    },
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowForm {
    pub id: Uuid,
    pub run_id: Uuid,
    pub timing: FlowFormTiming,
    #[schema(min_items = 1, max_items = 24)]
    pub questions: Vec<FlowFormQuestion>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowFormTiming {
    Waiting,
    Deferred,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowFormQuestion {
    pub prompt: String,
    pub input: FlowFormInput,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowFormInput {
    Confirm,
    SingleSelect {
        #[schema(max_items = 48)]
        options: Vec<String>,
    },
    MultiSelect {
        #[schema(max_items = 48)]
        options: Vec<String>,
        min: Option<usize>,
        max: Option<usize>,
    },
    Text {
        placeholder: Option<String>,
        multiline: bool,
    },
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowCompactReview {
    pub id: Uuid,
    pub summary: String,
    pub slice_preview: Option<String>,
    pub slice_count: usize,
    pub range_start: usize,
    pub range_end: usize,
    pub tokens_before: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowInteractionOperation {
    pub flow: FlowAddress,
    pub head: HistoryHead,
    pub host_generation: String,
    pub interaction_id: Uuid,
    pub request_id: Uuid,
    pub kind: FlowInteractionKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowInteractionKind {
    WaitingForm,
    DeferredForm,
    CompactReview,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyFlowInteractionRequest {
    pub operation: FlowInteractionOperation,
    pub reply: FlowInteractionReply,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowInteractionReply {
    AnswerForm {
        #[schema(min_items = 1, max_items = 24)]
        answers: Vec<FlowFormAnswer>,
    },
    RejectForm,
    AcceptSummary,
    EditSummary {
        summary: String,
    },
    RejectSummary,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowFormAnswer {
    Confirm {
        value: bool,
    },
    Select {
        index: usize,
    },
    MultiSelect {
        #[schema(max_items = 48)]
        indices: Vec<usize>,
    },
    Text {
        text: String,
    },
    Skip,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowInteractionReplyResponse {
    pub operation: FlowInteractionOperation,
    pub outcome: FlowInteractionOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowInteractionOutcome {
    Pending,
    Resumed,
    Dismissed,
    Deferred {
        message_id: Uuid,
        accepted: FlowCursor,
    },
    Decided,
    InvalidAnswer,
    Unavailable,
    Unknown,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowCompactResultResponse {
    pub operation: FlowInteractionOperation,
    pub result: FlowCompactResult,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowCompactResult {
    Applying,
    Applied { checkpoint: FlowCursor },
    Retained,
    Unconfirmed,
}
