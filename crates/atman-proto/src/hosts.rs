//! Host discovery and coordinated Session ownership transfer.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{SessionId, flows::FlowCursor};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostRole {
    Tui,
    Daemon,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostPresentation {
    Attached,
    Detached,
    Headless,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HostDescription {
    pub generation: String,
    pub runtime_generation: Uuid,
    pub role: HostRole,
    pub sessions: Vec<SessionId>,
    pub profile_owner: bool,
    pub presentation: HostPresentation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffSessionRequest {
    pub session_id: SessionId,
    pub request_id: Uuid,
    pub source_generation: Uuid,
    pub destination_generation: Uuid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HandoffScheduling {
    Running,
    Stopped,
    Failed { error: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HandoffFlow {
    pub instance_id: Uuid,
    pub cursor: FlowCursor,
    pub scheduling: Option<HandoffScheduling>,
}

/// Original durable boundaries; a released receipt is not destination installation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SessionHandoffReceipt {
    pub session_id: SessionId,
    pub request_id: Uuid,
    pub source_generation: Uuid,
    pub destination_generation: Uuid,
    pub catalog: FlowCursor,
    pub flows: Vec<HandoffFlow>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffBlocker {
    Admission,
    Active { instance_id: Uuid },
    Resources { instance_id: Uuid },
    Interactions { instance_id: Uuid },
    Transient { instance_id: Uuid },
    Retention,
    SourceCapture,
    ProfileAuthority,
    Controls,
    Observation { instance_id: Uuid },
    Continuation { instance_id: Uuid },
    Unavailable { message: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionHandoffOutcome {
    Pending {
        request_id: Uuid,
        source_generation: Uuid,
        session_id: SessionId,
        blockers: Vec<HandoffBlocker>,
    },
    Released {
        receipt: SessionHandoffReceipt,
    },
    Installed {
        receipt: SessionHandoffReceipt,
    },
}
