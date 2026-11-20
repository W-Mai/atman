//! Typed authored input queue controls for one explicit Flow.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    flows::{FlowAddress, FlowCursor},
    history::HistoryHead,
};

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFlowInboxRequest {
    pub flow: FlowAddress,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ReadFlowInboxResponse {
    pub flow: FlowAddress,
    pub cursor: FlowCursor,
    pub head: HistoryHead,
    pub items: Vec<FlowInboxItem>,
    /// Each entry is the complete, independent order for one original sender.
    pub next_turn_lanes: Vec<Vec<Uuid>>,
    pub offset: usize,
    pub total: usize,
    pub edits_available: bool,
    pub insertion_available: bool,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct FlowInboxItem {
    pub id: Uuid,
    pub revision: FlowCursor,
    pub text: String,
    pub text_truncated: bool,
    pub image_count: usize,
    pub has_quote: bool,
    pub delivery: FlowInboxDelivery,
    pub held: bool,
    pub separate_turn: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowInboxDelivery {
    NextTurn,
    AtBoundary,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowInboxMove {
    Earlier,
    Later,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowInboxChange {
    EditText {
        text: String,
    },
    SetDelivery {
        delivery: FlowInboxDelivery,
    },
    Move {
        direction: FlowInboxMove,
        expected_order: Vec<Uuid>,
    },
    Cancel,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EditFlowInboxRequest {
    pub flow: FlowAddress,
    pub expected_head: HistoryHead,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub expected_revision: FlowCursor,
    pub change: FlowInboxChange,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct EditFlowInboxResponse {
    pub flow: FlowAddress,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub receipt: FlowCursor,
}
