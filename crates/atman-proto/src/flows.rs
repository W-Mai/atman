//! Persistent session and flow addressing for daemon clients.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::SessionId;

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
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ControlFlowRequest {
    pub flow: FlowAddress,
    pub expected_revision: u64,
    pub action: FlowControl,
}
