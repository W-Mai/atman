//! Persistent session and flow addressing for daemon clients.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::SessionId;
use std::collections::BTreeMap;

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
    CancelTurn,
    Recover,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ControlFlowRequest {
    pub flow: FlowAddress,
    pub expected_revision: u64,
    pub action: FlowControl,
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
    pub arguments: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub invocation: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct InvokeFlowRequest {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    #[serde(default)]
    pub arguments: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub invocation: BTreeMap<String, serde_json::Value>,
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
