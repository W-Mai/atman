//! User permission review over one explicit Flow.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{FlowRunId, PermissionGroupView, flows::FlowAddress};

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFlowApprovalQueueRequest {
    pub flow: FlowAddress,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FlowApprovalQueueResponse {
    Ready {
        flow: FlowAddress,
        /// The owner returns a typed receipt from the User resolve RPC.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision_receipts: Option<bool>,
        requests: Vec<FlowApprovalRow>,
        groups: Vec<PermissionGroupView>,
    },
    Unavailable {
        flow: FlowAddress,
    },
    Capacity {
        flow: FlowAddress,
    },
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowApprovalRow {
    pub request_id: Uuid,
    pub revision: u64,
    pub requesting_run_id: FlowRunId,
    /// Absence means the original ID exceeded the review projection budget.
    pub tool_use_id: Option<String>,
    pub tool: String,
    pub tier: ApprovalTier,
    pub detail: FlowApprovalDetail,
    /// Exact arguments for a same-path grant. Absent when unavailable or unsafe to display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_relative_grant_path: Option<String>,
    /// Whether the original tool identity is available for a same-tool grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_tool_grant: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTier {
    Zero,
    One,
    Two,
    Three,
    Four,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowApprovalDetail {
    pub intent: Option<String>,
    pub preview: Option<String>,
    pub cwd: Option<String>,
    pub targets: Vec<String>,
    pub path_origin: Option<ApprovalPathOrigin>,
    pub risks: Vec<ApprovalRisk>,
    pub network: bool,
    pub execution_boundary: Option<ApprovalExecutionBoundary>,
    /// Some review content is unavailable because it exceeded the projection budget.
    pub incomplete: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalExecutionBoundary {
    Direct,
    Sandboxed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRisk {
    WorkspaceExternal,
    Network,
    Irreversible,
    FilesystemWrite,
    ProcessSpawn,
    RepositoryMutation,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPathOrigin {
    Omitted,
    Relative,
    ExplicitInside,
    ExplicitExternal,
    Unbound,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionAction {
    Approved,
    Denied,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ApprovalDecisionView {
    pub decision_id: Uuid,
    pub action: ApprovalDecisionAction,
}
