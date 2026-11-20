//! Public execution observations and control receipts.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::flows::{FlowAddress, FlowControl, FlowCursor};

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowExecutionRequest {
    pub flow: FlowAddress,
    /// An original single-target stop or recover request. This lookup never retries a control.
    pub request_id: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowExecutionPhase {
    Prepared,
    Idle,
    Running,
    Waiting,
    Finishing,
    Cancelling,
    DetachedActive,
    Interrupted,
    Closing,
    Closed,
}

impl FlowExecutionPhase {
    pub fn observe<I>(state: &atman_rt::InstanceState<I>, loaded: bool) -> Self {
        use atman_rt::{InstanceActivity, InstanceState, TurnPhase};

        match state {
            InstanceState::Prepared => Self::Prepared,
            InstanceState::Open(InstanceActivity::Idle) => Self::Idle,
            InstanceState::Open(InstanceActivity::Active(_)) if !loaded => Self::DetachedActive,
            InstanceState::Open(InstanceActivity::Active(active)) => match active.phase {
                TurnPhase::Running => Self::Running,
                TurnPhase::Waiting(_) => Self::Waiting,
                TurnPhase::Finishing => Self::Finishing,
                TurnPhase::Cancelling => Self::Cancelling,
            },
            InstanceState::Open(InstanceActivity::Interrupted { .. }) => Self::Interrupted,
            InstanceState::Closing { .. } => Self::Closing,
            InstanceState::Closed => Self::Closed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowInterruption {
    Cancelled,
    Failed,
    HostLost,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowExecutionResponse {
    pub flow: FlowAddress,
    pub cursor: FlowCursor,
    pub revision: u64,
    /// Detached facts do not imply a running process.
    pub loaded: bool,
    pub paused: bool,
    pub phase: FlowExecutionPhase,
    pub interruption: Option<FlowInterruption>,
    pub allowed_actions: Vec<FlowControl>,
    pub request_id: Option<Uuid>,
    pub control: Option<FlowExecutionControlReceipt>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowExecutionControlReceipt {
    pub action: FlowControl,
    pub accepted: FlowCursor,
    /// Stop cites the matching turn's completion, never the latest unrelated state.
    pub applied: Option<FlowCursor>,
}

/// Schema of the retained Profile batch record. The existing wire names are preserved.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowControlBatchRecord {
    pub id: Uuid,
    pub sender: FlowControlSender,
    pub submitted: FlowCursor,
    pub items: Vec<FlowControlBatchItem>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum FlowControlSender {
    User(Uuid),
    Flow(FlowAddress),
    Host(Uuid),
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowControlBatchItem {
    pub request: FlowControlBatchCommand,
    pub history: Vec<FlowControlProgressRecord>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowControlBatchCommand {
    pub id: Uuid,
    pub sender: FlowControlSender,
    pub target: FlowAddress,
    pub expected_revision: u64,
    pub pause_key: Uuid,
    pub action: FlowControl,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowControlProgressRecord {
    pub progress: FlowControlProgress,
    pub recorded: FlowCursor,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FlowControlProgress {
    Accepted { receipt: FlowCursor },
    Applied { receipt: FlowCursor },
    Failed { reason: String },
}
