//! Committed facts for one Flow's overview.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::flows::{FlowAddress, FlowCursor};

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowOverviewResponse {
    pub flow: FlowAddress,
    pub cursor: FlowCursor,
    pub project_root: Option<OverviewText>,
    pub goal: Option<OverviewGoal>,
    pub planning: OverviewPlanning,
    /// Recorded intent only; this does not confirm a worktree exists or is held.
    pub workspace: Option<OverviewWorkspace>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewText {
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewGoal {
    pub id: String,
    pub revision: u64,
    pub objective: OverviewText,
    pub status: OverviewGoalStatus,
    pub pause_reason: Option<OverviewGoalPauseReason>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverviewGoalStatus {
    Active,
    Paused,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverviewGoalPauseReason {
    UserRequested,
    ParentRequested,
    Completed,
    NeedsInput,
    Blocked,
    Cancelled,
    RunFailed,
    Interrupted,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewPlanning {
    pub revision: u64,
    pub plan: Option<OverviewPlan>,
    pub todos: Vec<OverviewTodo>,
    /// Counts cover all Todos, even when the item list is truncated.
    pub todo_counts: OverviewTodoCounts,
    pub todos_truncated: bool,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewPlan {
    pub id: OverviewText,
    pub title: OverviewText,
    pub steps: Vec<OverviewPlanStep>,
    pub done: usize,
    pub total: usize,
    pub steps_truncated: bool,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewPlanStep {
    pub index: usize,
    pub text: OverviewText,
    pub done: bool,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewTodo {
    pub id: Uuid,
    #[serde(rename = "where")]
    pub where_: OverviewText,
    pub why: OverviewText,
    pub how: OverviewText,
    pub expected_result: OverviewText,
    pub status: OverviewTodoStatus,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewTodoCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub done: usize,
    pub cancelled: usize,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverviewTodoStatus {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct OverviewWorkspace {
    pub path: OverviewText,
    pub kind: OverviewWorkspaceKind,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverviewWorkspaceKind {
    Allocate,
    Share,
}
