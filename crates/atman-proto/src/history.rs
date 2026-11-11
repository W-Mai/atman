//! Conversation selection identities for client observations and write admission.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

pub const HISTORY_ADMISSION_ERROR: i32 = -32002;

/// A selected conversation generation, independent of execution and journal revisions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryHead {
    pub branch: Uuid,
    pub version: u64,
}

/// Fresh input was rejected before acceptance; an accepted retry retains its original receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryAdmissionError {
    Required {
        current: HistoryHead,
    },
    Changed {
        expected: HistoryHead,
        current: HistoryHead,
    },
}

use crate::{
    flows::{FlowAddress, FlowCursor},
    responses::InputTurn,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistorySource {
    pub flow: FlowAddress,
    pub branch: Uuid,
    pub cursor: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryBranchesRequest {
    pub flow: FlowAddress,
    pub expected_head: HistoryHead,
    pub offset: u64,
    /// Number of rows, between 1 and 100.
    pub limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryBranch {
    pub id: Uuid,
    pub parent: Option<HistorySource>,
    pub created: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryBranchesResponse {
    pub flow: FlowAddress,
    pub head: HistoryHead,
    pub observed: FlowCursor,
    pub branches: Vec<HistoryBranch>,
    pub total: u64,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationCutoff {
    BeforeUserTurn {
        lease: InputTurn,
        input: Uuid,
        receipt: FlowCursor,
    },
    AfterCompletedTurn {
        lease: InputTurn,
        receipt: FlowCursor,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryTarget {
    Revert {
        observed: HistorySource,
        cutoff: Box<ConversationCutoff>,
        branch: Uuid,
    },
    Restore {
        branch: Uuid,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryReviewRequest {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    pub expected_head: HistoryHead,
    pub target: HistoryTarget,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryGoalVersion {
    pub id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryQueueItem {
    pub message: Uuid,
    pub revision: FlowCursor,
}

/// Submit exactly the reviewed request; sender identity is supplied by authentication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryOperationRequest {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    pub expected_head: HistoryHead,
    pub execution_revision: u64,
    pub active: Option<InputTurn>,
    pub goal: Option<HistoryGoalVersion>,
    pub queued: Vec<HistoryQueueItem>,
    pub target: HistoryTarget,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistorySelection {
    Create { branch: Uuid, parent: HistorySource },
    Restore { branch: Uuid },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryReviewResponse {
    pub request: HistoryOperationRequest,
    pub selection: HistorySelection,
    pub affected_turns: Option<usize>,
    pub admitted_inputs: Vec<Uuid>,
    #[serde(default)]
    pub recoverable_inputs: Vec<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryOperationQuery {
    pub flow: FlowAddress,
    pub request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HistoryOperationStatus {
    Preparing,
    Stopping,
    Ready {
        receipt: FlowCursor,
        completion: Option<FlowCursor>,
    },
    Aborted {
        receipt: FlowCursor,
    },
    Applied {
        receipt: FlowCursor,
        head: HistoryHead,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryCancellation {
    pub request_id: Uuid,
    pub receipt: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryOperationRecord {
    pub request: HistoryOperationRequest,
    pub selection: HistorySelection,
    pub accepted: FlowCursor,
    pub cancellation: Option<HistoryCancellation>,
    pub status: HistoryOperationStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryStatusResponse {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    pub operation: Option<HistoryOperationRecord>,
}
