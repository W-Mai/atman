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

/// Exclusive position of a complete flow commit. The server validates its journal identity and digest against retained history.
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
