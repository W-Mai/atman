//! Host-local Quick Ask admission, observation and reconciliation.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    conversation::ConversationItem,
    execution::FlowExecutionPhase,
    flows::{FlowAddress, FlowCursor, SubmittedUserContent},
    history::{HistoryHead, HistorySource},
    responses::InputTurn,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionIdentity {
    pub source: FlowAddress,
    pub question_id: Uuid,
    pub request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StartQuestionRequest {
    pub identity: QuestionIdentity,
    /// Session owner identity from question.status.
    pub owner_generation: Uuid,
    pub expected_head: HistoryHead,
    pub observed: HistorySource,
    pub user: SubmittedUserContent,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionStatusRequest {
    pub identity: QuestionIdentity,
    pub owner_generation: Option<Uuid>,
}

/// A position in the temporary question on its original runtime host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionPosition {
    pub host_generation: Uuid,
    pub flow_id: Uuid,
    pub sequence: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionStartReceipt {
    pub source: HistorySource,
    pub launch: QuestionPosition,
    pub accepted: QuestionPosition,
    pub message_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StartQuestionResponse {
    pub identity: QuestionIdentity,
    pub owner_generation: Uuid,
    pub receipt: QuestionStartReceipt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionStartState {
    Unseen,
    Preparing,
    Retryable,
    Accepted { receipt: Box<QuestionStartReceipt> },
    Retired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionStatusResponse {
    pub identity: QuestionIdentity,
    pub owner_generation: Option<Uuid>,
    pub attempt: QuestionStartState,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadQuestionTranscriptRequest {
    pub identity: QuestionIdentity,
    pub owner_generation: Uuid,
    pub host_generation: Uuid,
    pub after: Option<QuestionPosition>,
    /// Maximum commits per page, from 1 to 32.
    pub limit: u32,
    /// At the exact tail, wait briefly for a commit. Initial reads always return immediately.
    #[serde(default)]
    pub wait: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionTranscriptBatch {
    pub receipt: QuestionPosition,
    /// May be empty for a commit without visible messages.
    pub items: Vec<ConversationItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionTranscriptState {
    Ready {
        batches: Vec<QuestionTranscriptBatch>,
        next: Option<QuestionPosition>,
        has_more: bool,
    },
    Capacity {
        required_at_least: usize,
        allowed: usize,
    },
    ReadersBusy,
    Unavailable,
    Retired,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadQuestionTranscriptResponse {
    pub identity: QuestionIdentity,
    pub owner_generation: Uuid,
    pub host_generation: Uuid,
    pub transcript: QuestionTranscriptState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionTarget {
    pub identity: QuestionIdentity,
    pub owner_generation: Uuid,
    pub host_generation: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionFollowupRequest {
    pub question: QuestionTarget,
    pub followup_request_id: Uuid,
    /// The last installed publication when this follow-up was authored.
    pub observed: QuestionPosition,
    pub user: SubmittedUserContent,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionFollowupResponse {
    pub question: QuestionTarget,
    pub followup_request_id: Uuid,
    pub message_id: Uuid,
    pub accepted: QuestionPosition,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionFollowupStatusRequest {
    pub question: QuestionTarget,
    pub followup_request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionInputConsumption {
    Pending,
    Cancelled {
        receipt: QuestionPosition,
    },
    Committed {
        receipt: QuestionPosition,
        lease: InputTurn,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionInputObservation {
    pub message_id: Uuid,
    pub accepted: QuestionPosition,
    pub observed_receipt: QuestionPosition,
    pub input: QuestionInputConsumption,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionFollowupState {
    Unavailable,
    Retired,
    /// A sampled lookup. An in-flight follow-up may still be accepted.
    Unseen,
    Observed {
        observation: Box<QuestionInputObservation>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionFollowupStatusResponse {
    pub question: QuestionTarget,
    pub followup_request_id: Uuid,
    pub followup: QuestionFollowupState,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionExecutionRequest {
    pub question: QuestionTarget,
    pub stop_request_id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionStopReceipt {
    pub accepted: QuestionPosition,
    /// The matching turn's completion after cleanup.
    pub applied: Option<QuestionPosition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionExecutionObservation {
    pub cursor: QuestionPosition,
    pub revision: u64,
    pub phase: FlowExecutionPhase,
    /// Only installed while the current turn can accept Stop.
    pub stoppable: Option<InputTurn>,
    pub stop: Option<QuestionStopReceipt>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionExecutionState {
    Unavailable,
    Retired,
    Observed {
        observation: Box<QuestionExecutionObservation>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionExecutionResponse {
    pub question: QuestionTarget,
    pub stop_request_id: Option<Uuid>,
    pub execution: QuestionExecutionState,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionStopRequest {
    pub question: QuestionTarget,
    pub stop_request_id: Uuid,
    pub expected_revision: u64,
    pub lease: InputTurn,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionStopResponse {
    pub question: QuestionTarget,
    pub stop_request_id: Uuid,
    pub accepted: QuestionPosition,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionDiscardRequest {
    pub question: QuestionTarget,
    pub discard_request_id: Uuid,
    pub observed: QuestionPosition,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionDiscardStatusRequest {
    pub question: QuestionTarget,
    pub discard_request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionDiscardState {
    Unseen,
    Draining,
    Retryable,
    Discarded,
    Retired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionDiscardResponse {
    pub question: QuestionTarget,
    pub discard_request_id: Uuid,
    pub discard: QuestionDiscardState,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRefreshRequest {
    pub question: QuestionTarget,
    pub refresh_request_id: Uuid,
    /// Exact temporary tail when the question's context can be changed.
    pub expected: QuestionPosition,
    pub source_head: HistoryHead,
    pub observed_source: HistorySource,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRefreshStatusRequest {
    pub question: QuestionTarget,
    pub refresh_request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRefreshReceipt {
    pub source: HistorySource,
    pub applied: QuestionPosition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRefreshResponse {
    pub question: QuestionTarget,
    pub refresh_request_id: Uuid,
    pub receipt: QuestionRefreshReceipt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionRefreshState {
    /// A sampled lookup. An in-flight refresh may still commit.
    Unseen,
    Unavailable,
    Retired,
    Applied {
        receipt: Box<QuestionRefreshReceipt>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRefreshStatusResponse {
    pub question: QuestionTarget,
    pub refresh_request_id: Uuid,
    pub refresh: QuestionRefreshState,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionKeepRequest {
    pub question: QuestionTarget,
    pub keep_request_id: Uuid,
    pub expected: QuestionPosition,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionKeepStatusRequest {
    pub question: QuestionTarget,
    pub keep_request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionKeepInstalledRequest {
    pub question: QuestionTarget,
    pub keep_request_id: Uuid,
    /// The exact retained boundary verified when installing the durable window.
    pub installed: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionKeepReceipt {
    pub flow: FlowAddress,
    pub source: QuestionPosition,
    pub destination: FlowCursor,
    pub outputs_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionKeepState {
    Unseen,
    Pending,
    Published { receipt: Box<QuestionKeepReceipt> },
    Installed { receipt: Box<QuestionKeepReceipt> },
    Retired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionKeepResponse {
    pub question: QuestionTarget,
    pub keep_request_id: Uuid,
    pub keep: QuestionKeepState,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionModelCheckRequest {
    pub question: QuestionTarget,
    pub expected: QuestionPosition,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct QuestionModelCheckResponse {
    pub question: QuestionTarget,
    pub checked: QuestionPosition,
    /// The resolved model key for this temporary question.
    pub model: String,
}
