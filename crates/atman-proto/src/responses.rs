//! Public response DTOs for durable daemon operations.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    SessionId, SessionSummary,
    flows::{FlowAddress, FlowCursor},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct PingResponse {
    pub pong: bool,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct CancelRunResponse {
    pub cancelled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ResolvePromptResponse {
    pub resolved: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ListSessionsRequest {
    pub project_root: Option<String>,
    pub search: Option<String>,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(transparent)]
pub struct ListSessionsResponse(pub Vec<SessionSummary>);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SessionView {
    pub session_id: SessionId,
    pub title: Option<String>,
    pub project_root: Option<String>,
    pub default_flow: Option<Uuid>,
    pub flow_count: usize,
    pub catalog: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SchedulingView {
    Running,
    Stopped,
    Failed { error: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BindFlowResponse {
    pub flow: FlowAddress,
    pub scheduler: SchedulingView,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ControlFlowResponse {
    pub request_id: Uuid,
    /// A committed control receipt, not proof that execution has finished.
    pub receipt: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InputDisposition {
    Pending,
    InputCommitted { by: FlowCursor },
    Cancelled { by: FlowCursor },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowAcceptance {
    pub message_id: Uuid,
    pub receipt: FlowCursor,
    pub disposition: InputDisposition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct InvokeFlowResponse {
    pub acceptance: FlowAcceptance,
}

/// Exact invocation that consumed an input; consumption does not imply completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct InputTurn {
    pub flow: FlowAddress,
    pub turn: Uuid,
    pub run: Uuid,
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InputConsumption {
    Pending,
    Cancelled {
        receipt: FlowCursor,
    },
    Committed {
        receipt: FlowCursor,
        lease: InputTurn,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowInputObservation {
    pub message_id: Uuid,
    pub accepted: FlowCursor,
    pub input: InputConsumption,
    pub observed_receipt: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct InputStatusResponse {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    /// Absence is a sampled lookup, not proof that a concurrent request cannot commit.
    pub observation: Option<FlowInputObservation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct LaunchFlowResponse {
    pub flow: FlowAddress,
    pub receipt: FlowCursor,
    pub acceptance: Option<FlowAcceptance>,
}

/// A bounded page of committed final-answer publications.
///
/// Message bodies remain opaque JSON because their concrete parts belong to the
/// runtime message model. The page envelope is stable and can be consumed by
/// clients without depending on that private model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowOutputPage {
    pub messages: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Uuid>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowOutputArtifact {
    pub id: String,
    pub bytes: usize,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowOutputDetail {
    pub publication_id: Uuid,
    pub receipt: crate::flows::FlowCursor,
    pub artifacts: Vec<FlowOutputArtifact>,
    pub has_artifact: bool,
}

/// A bounded page of committed Flow journal batches.
///
/// Fact bodies remain opaque JSON because their concrete runtime types are not
/// part of the shared client contract. Durable cursors retain their journal
/// identity and digest for resumable reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowEventPage {
    pub batches: Vec<FlowEventBatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<crate::flows::FlowCursor>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowEventBatch {
    pub receipt: crate::flows::FlowCursor,
    pub facts: Vec<serde_json::Value>,
}

/// A bounded page of registered Flow instances in a Session.
///
/// Execution and activity bodies remain opaque JSON so shared clients can
/// browse the catalog without depending on private runtime types.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowListPage {
    pub instances: Vec<FlowListEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Uuid>,
    pub total: usize,
    pub catalog: FlowCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowListEntry {
    pub flow: FlowAddress,
    pub entry: Option<String>,
    pub execution: Option<serde_json::Value>,
    pub unavailable: Option<String>,
    pub loaded: bool,
    pub trailing_bytes: Option<u64>,
    pub archived: bool,
    pub activity: Option<serde_json::Value>,
}
