//! First-input admission and reconciliation for host-local Quick Ask dialogs.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    flows::{FlowAddress, SubmittedUserContent},
    history::{HistoryHead, HistorySource},
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
