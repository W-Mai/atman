use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    CompactionProjection, ContextProjection, DaemonGeneration, EventCursor, FlowRunId,
    InteractionProjection, PlanProjection, ResourceProjection, Revision, RunProjection, SessionId,
    SessionLifecycle, SessionMetadataProjection, TodoProjection, TranscriptItem, TrustProjection,
    TurnId, UsageProjection, WorkflowProjection,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
pub struct TimelineItemId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
pub struct TimelineSegmentId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TimelineCursor {
    pub seq: u64,
    pub item_id: TimelineItemId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TimelineDetailRef {
    pub item_id: TimelineItemId,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimelineItemKind {
    Message,
    Diff,
    FileEdit,
    ActivitySummary,
    Compaction,
    Mermaid,
    Notice,
    Extension,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct TimelineItem {
    pub id: TimelineItemId,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<FlowRunId>,
    pub kind: TimelineItemKind,
    pub preview: TranscriptItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<TimelineDetailRef>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimelineTurnState {
    Active,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct TimelineTurnSegment {
    pub id: TimelineSegmentId,
    pub turn_id: TurnId,
    pub start_seq: u64,
    pub latest_seq: u64,
    pub revision: Revision,
    pub state: TimelineTurnState,
    #[serde(default)]
    pub items: Vec<TimelineItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct TimelineSessionSegment {
    pub id: TimelineSegmentId,
    pub start_seq: u64,
    pub latest_seq: u64,
    pub revision: Revision,
    #[serde(default)]
    pub items: Vec<TimelineItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineSegment {
    Turn { segment: TimelineTurnSegment },
    Session { segment: TimelineSessionSegment },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TimelineRemaining {
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_segments: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct TimelineLiveState {
    pub head_complete: bool,
    pub metadata: SessionMetadataProjection,
    pub lifecycle: SessionLifecycle,
    #[serde(default)]
    pub runs: Vec<RunProjection>,
    #[serde(default)]
    pub active_turns: Vec<TurnId>,
    #[serde(default)]
    pub compactions: Vec<CompactionProjection>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub todos: Vec<TodoProjection>,
    #[serde(default)]
    pub plans: Vec<PlanProjection>,
    #[serde(default)]
    pub context: ContextProjection,
    #[serde(default)]
    pub trust: TrustProjection,
    pub interactions: InteractionProjection,
    #[serde(default)]
    pub resources: Vec<ResourceProjection>,
    #[serde(default)]
    pub usage: UsageProjection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct SessionTimelinePage {
    pub session_id: SessionId,
    pub daemon_generation: DaemonGeneration,
    pub as_of_cursor: EventCursor,
    pub projection_revision: Revision,
    #[serde(default)]
    pub segments: Vec<TimelineSegment>,
    pub older: TimelineRemaining,
    pub newer: TimelineRemaining,
    pub serialized_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<TimelineLiveState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SessionTimelineBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_budget: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct GetSessionTimelineTailRequest {
    pub session_id: SessionId,
    #[serde(flatten)]
    pub budget: SessionTimelineBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct GetSessionTimelineBeforeRequest {
    pub session_id: SessionId,
    pub before: TimelineCursor,
    #[serde(flatten)]
    pub budget: SessionTimelineBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct GetSessionTimelineAfterRequest {
    pub session_id: SessionId,
    pub after: TimelineCursor,
    #[serde(flatten)]
    pub budget: SessionTimelineBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct GetSessionTimelineAroundRequest {
    pub session_id: SessionId,
    pub anchor: TimelineCursor,
    #[serde(flatten)]
    pub budget: SessionTimelineBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SearchSessionHistoryRequest {
    pub session_id: SessionId,
    pub query: String,
    #[serde(default)]
    pub project_wide: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SessionHistorySearchHit {
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SearchSessionHistoryResponse {
    #[serde(default)]
    pub hits: Vec<SessionHistorySearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct GetSessionTimelineItemDetailRequest {
    pub session_id: SessionId,
    pub item_id: TimelineItemId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct SessionTimelineItemDetail {
    pub session_id: SessionId,
    pub item_id: TimelineItemId,
    pub item: TranscriptItem,
}
