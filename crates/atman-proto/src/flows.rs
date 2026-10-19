//! Persistent session and flow addressing for daemon clients.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::SessionId;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManagementAction {
    Discover,
    ReadOutput,
    ReadContext,
    Send,
    Watch,
    Control,
    ManageGoal,
    Delegate,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum ManagementTarget {
    Flow(FlowAddress),
    Session(SessionId),
    AllSessions,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ManagementGrant {
    pub id: Uuid,
    /// Registered instance in the selected management Session.
    pub instance_id: Uuid,
    pub target: ManagementTarget,
    pub actions: std::collections::BTreeSet<ManagementAction>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct SelectManagementRequest {
    pub request_id: Uuid,
    pub expected: FlowCursor,
    pub session_id: SessionId,
    pub grants: Vec<ManagementGrant>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowAddress {
    pub session_id: SessionId,
    pub instance_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct CreateSessionRequest {
    pub session_id: SessionId,
    pub title: String,
    /// An absent project is an explicit projectless container.
    pub project_root: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct SessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowRequest {
    pub flow: FlowAddress,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ListFlowsRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub before: Option<Uuid>,
    #[serde(default = "page_size")]
    pub limit: usize,
}

fn page_size() -> usize {
    20
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    Pause,
    Resume,
    /// Release one explicitly selected pause without removing other controllers' gates.
    ReleasePause,
    CancelTurn,
    Recover,
    /// Accept closure; the execution state becomes closed after resource cleanup.
    Close,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlFlowRequest {
    pub flow: FlowAddress,
    /// Reuse with identical parameters to retrieve the original committed receipt.
    pub request_id: Uuid,
    pub expected_revision: u64,
    pub action: FlowControl,
    /// Required only for release_pause. Read the key from flow.get execution.pause_keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_key: Option<Uuid>,
}

/// Persist the entire target list before independently delivering each control.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlFlowBatchRequest {
    pub request_id: Uuid,
    /// Each target has one stable control identity, independent of the batch identity.
    pub commands: Vec<ControlFlowRequest>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlFlowStatusRequest {
    pub request_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowUserContent {
    pub text: String,
    #[serde(default)]
    pub images: Vec<crate::InlineImage>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct LaunchFlowRequest {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    pub expected_catalog_sequence: u64,
    /// Absolute source path. Source contents are retained at creation.
    pub path: String,
    pub entry: String,
    #[serde(default)]
    pub start: FlowStart,
    /// Optional objective installed atomically before entry execution.
    pub initial_goal: Option<String>,
    #[serde(default)]
    pub arguments: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub invocation: BTreeMap<String, serde_json::Value>,
    pub user: Option<FlowUserContent>,
}

/// An entry can be ready to receive input without creating an initial turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowStart {
    #[default]
    Invoke,
    Waiting,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct InvokeFlowRequest {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    #[serde(default)]
    pub arguments: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub invocation: BTreeMap<String, serde_json::Value>,
    pub user: Option<FlowUserContent>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ReadFlowOutputRequest {
    pub flow: FlowAddress,
    /// Last publication UUID received, exclusive.
    #[serde(default)]
    pub after: Option<Uuid>,
    #[serde(default = "page_size")]
    pub limit: usize,
}

/// Exclusive position of a complete journal commit. The server validates its identity and digest against retained history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowCursor {
    pub journal_id: Uuid,
    pub sequence: u64,
    pub digest: String,
}

impl std::fmt::Display for FlowCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.journal_id, self.sequence, self.digest)
    }
}

impl std::str::FromStr for FlowCursor {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = "expected flow cursor journal UUID:sequence:SHA-256 digest";
        let mut fields = value.split(':');
        let journal_id: Uuid = fields.next().ok_or(invalid)?.parse().map_err(|_| invalid)?;
        let sequence = fields.next().ok_or(invalid)?.parse().map_err(|_| invalid)?;
        let digest = fields.next().ok_or(invalid)?;
        if journal_id.is_nil()
            || sequence == 0
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || fields.next().is_some()
        {
            return Err(invalid);
        }
        Ok(Self {
            journal_id,
            sequence,
            digest: digest.into(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ReadFlowEventsRequest {
    pub flow: FlowAddress,
    #[serde(default)]
    pub after: Option<FlowCursor>,
    #[serde(default = "page_size")]
    pub limit: usize,
}
