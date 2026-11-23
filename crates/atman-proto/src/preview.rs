//! Compact preview from an existing Flow execution owner.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{flows::FlowAddress, history::HistoryHead, responses::InputTurn};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFlowPreviewRequest {
    pub flow: FlowAddress,
    /// A matching edition waits for a change. Omit for an immediate snapshot.
    pub known: Option<FlowPreviewEdition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowPreviewEdition {
    pub host_generation: Uuid,
    pub worker_incarnation: Uuid,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReadFlowPreviewResponse {
    Ready {
        snapshot: Box<FlowPreviewSnapshot>,
    },
    Unchanged {
        flow: FlowAddress,
        edition: FlowPreviewEdition,
    },
    Unavailable {
        flow: FlowAddress,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowPreviewSnapshot {
    pub flow: FlowAddress,
    pub edition: FlowPreviewEdition,
    pub head: HistoryHead,
    #[schema(max_length = 512)]
    pub text: String,
    pub active: Option<InputTurn>,
    pub completion: Option<FlowPreviewCompletion>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowPreviewCompletion {
    pub lease: InputTurn,
    pub outcome: FlowPreviewOutcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowPreviewOutcome {
    Completed,
    Cancelled,
    Failed,
    HostLost,
}
