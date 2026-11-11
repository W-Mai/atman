//! Conversation selection identities for client observations and write admission.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

pub const HISTORY_ADMISSION_ERROR: i32 = -32002;

/// A selected conversation generation, independent of execution and journal revisions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryHead {
    pub branch: Uuid,
    pub version: u64,
}

/// Fresh input was rejected before acceptance; an accepted retry retains its original receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryAdmissionError {
    Required {
        current: HistoryHead,
    },
    Changed {
        expected: HistoryHead,
        current: HistoryHead,
    },
}
