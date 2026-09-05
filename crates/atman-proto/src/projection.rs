use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    CompactionOperationId, DaemonGeneration, EventCursor, FlowRunId, InterjectionLevel,
    InterjectionState, NameSource, Revision, SessionId,
};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const PROJECTION_EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct TurnId(pub Uuid);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct ContextId(pub Uuid);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, format = Uuid)]
pub struct MessagePartId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(transparent)]
pub struct ResourceId(pub String);

impl ResourceId {
    pub fn task(task_id: Uuid) -> Self {
        Self(format!("task:{task_id}"))
    }

    pub fn task_id(&self) -> Option<Uuid> {
        self.0
            .strip_prefix("task:")
            .and_then(|id| Uuid::parse_str(id).ok())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct SessionSnapshot {
    pub schema_version: u32,
    pub daemon_generation: DaemonGeneration,
    pub cursor: EventCursor,
    pub projection: SessionProjection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct SessionProjection {
    pub revision: Revision,
    pub metadata: SessionMetadataProjection,
    pub lifecycle: SessionLifecycle,
    #[serde(default)]
    pub runs: Vec<RunProjection>,
    #[serde(default)]
    pub transcript: Vec<TranscriptItem>,
    #[serde(default)]
    pub workflows: Vec<WorkflowProjection>,
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
    #[serde(default)]
    pub interactions: InteractionProjection,
    #[serde(default)]
    pub resources: Vec<ResourceProjection>,
    #[serde(default)]
    pub usage: UsageProjection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SessionMetadataProjection {
    pub id: SessionId,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub name_source: NameSource,
    #[serde(default)]
    pub project_root: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    Creating,
    Idle,
    Active,
    Degraded,
    Closing,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct RunProjection {
    pub id: FlowRunId,
    /// Owning turn, when known. Older event logs may not contain this association.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default)]
    pub flow_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub parent_run_id: Option<FlowRunId>,
    #[serde(default)]
    pub parent_node_id: Option<String>,
    pub state: RunLifecycle,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunLifecycle {
    Queued,
    Starting,
    Running,
    WaitingInput,
    Cancelling,
    Cancelled,
    Succeeded,
    Failed,
    Lost,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptItem {
    Message {
        seq: u64,
        ts: DateTime<Utc>,
        #[serde(default)]
        run_id: Option<FlowRunId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_id: Option<ContextId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint_index: Option<usize>,
        message: MessageProjection,
    },
    Diff {
        seq: u64,
        ts: DateTime<Utc>,
        #[serde(default)]
        run_id: Option<FlowRunId>,
        #[serde(default)]
        tool_use_id: Option<String>,
        title: String,
        #[serde(default)]
        old_content: Option<String>,
        #[serde(default)]
        new_content: Option<String>,
        #[serde(default)]
        unified_diff: Option<String>,
    },
    FileEdit {
        seq: u64,
        ts: DateTime<Utc>,
        #[serde(default)]
        turn_id: Option<TurnId>,
        #[serde(default)]
        run_id: Option<FlowRunId>,
        #[serde(default)]
        tool_use_id: Option<String>,
        tool_name: String,
        path: String,
        added_lines: u64,
        removed_lines: u64,
        hunks: u64,
    },
    Compaction {
        seq: u64,
        ts: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<CompactionOperationId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_id: Option<ContextId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<FlowRunId>,
        #[serde(default)]
        outcome: CompactionOutcome,
        range_start: u64,
        range_end: u64,
        before_tokens: u64,
        after_tokens: u64,
        summary: String,
    },
    Mermaid {
        seq: u64,
        ts: DateTime<Utc>,
        source: String,
    },
    Notice {
        seq: u64,
        ts: DateTime<Utc>,
        level: NoticeLevel,
        text: String,
    },
    Extension {
        seq: u64,
        ts: DateTime<Utc>,
        kind: String,
        #[schema(value_type = Object)]
        payload: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactionOutcome {
    #[default]
    Finished,
    Failed,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct CompactionProjection {
    pub id: CompactionOperationId,
    pub started_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<ContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<FlowRunId>,
    pub range_start: u64,
    pub range_end: u64,
    pub before_tokens: u64,
    pub compacted_count: u64,
    #[serde(default)]
    pub summary: String,
    pub started_at: DateTime<Utc>,
}

impl TranscriptItem {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Message { seq, .. }
            | Self::Diff { seq, .. }
            | Self::FileEdit { seq, .. }
            | Self::Compaction { seq, .. }
            | Self::Mermaid { seq, .. }
            | Self::Notice { seq, .. }
            | Self::Extension { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Debug,
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct MessageProjection {
    pub role: MessageRole,
    pub origin: MessageOrigin,
    pub turn_id: TurnId,
    pub parts: Vec<MessagePart>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    User,
    Watcher,
    Interjection,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    ContextRecord {
        key: String,
        content: String,
        digest: String,
        revision: u64,
    },
    CompactSummary {
        summary: String,
        seq_start: u64,
        seq_end: u64,
        count: usize,
    },
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<MessagePartId>,
        media_type: String,
        #[serde(default)]
        artifact_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        detail: ImageDetail,
    },
    ToolUse {
        id: String,
        name: String,
        #[schema(value_type = Object)]
        input: serde_json::Value,
        #[serde(default)]
        intent: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    Low,
    High,
    Original,
    #[default]
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct WorkflowProjection {
    pub turn_id: TurnId,
    #[serde(default)]
    pub roots: Vec<WorkflowNodeProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct WorkflowNodeProjection {
    pub id: String,
    pub kind: WorkflowNodeKind,
    pub label: String,
    pub state: WorkflowNodeState,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub output_preview: Option<String>,
    #[serde(default)]
    #[schema(no_recursion)]
    pub children: Vec<WorkflowNodeProjection>,
    #[serde(default)]
    pub parallel: bool,
    #[serde(default)]
    pub approval: Option<ApprovalProjection>,
    #[serde(default)]
    pub llm_usage: Option<LlmUsageProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Flow {
        run_id: FlowRunId,
        flow_name: String,
    },
    Statement {
        kind: WorkflowStatementKind,
    },
    ToolCall {
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        #[serde(default)]
        intent: Option<String>,
        #[serde(default)]
        result_preview: Option<String>,
    },
    Subflow {
        run_id: FlowRunId,
        flow_name: String,
    },
    FanoutBranch {
        branch_index: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStatementKind {
    Llm {
        #[serde(default)]
        model: Option<String>,
    },
    ToolCall {
        path: String,
    },
    Fanout {
        collect: WorkflowFanoutMode,
    },
    UserConfirm,
    Subflow {
        name: String,
    },
    Message {
        role: String,
    },
    FixUntilTest,
    When {
        condition_preview: String,
    },
    Loop,
    Return,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFanoutMode {
    All,
    First,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApprovalProjection {
    Pending {
        level: String,
        #[serde(default)]
        preview: Option<String>,
    },
    Approved,
    Denied {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct LlmUsageProjection {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub call_purpose: LlmCallPurpose,
    #[serde(default)]
    pub call_scope: LlmCallScope,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub wallclock_ms: u64,
    #[serde(default)]
    pub ttft_ms: u64,
    #[serde(default)]
    pub tokens_per_second: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LlmCallPurpose {
    #[default]
    General,
    Classification,
    Extraction,
    BranchGeneration,
    Compaction,
    InterjectionClassification,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LlmCallScope {
    Root,
    Child,
    #[default]
    Detached,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TodoProjection {
    pub id: String,
    #[serde(rename = "where")]
    pub where_: String,
    pub why: String,
    pub how: String,
    pub expected_result: String,
    pub state: TodoState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct PlanProjection {
    pub id: String,
    pub title: String,
    pub steps: Vec<PlanStepProjection>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct PlanStepProjection {
    pub index: usize,
    pub text: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub done_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ContextProjection {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub window_tokens: u64,
    #[serde(default)]
    pub window_budget: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub last_ttft_ms: u64,
    #[serde(default)]
    pub last_tokens_per_second: f64,
    #[serde(default)]
    pub memory_recent_count: u16,
    #[serde(default)]
    pub usage_buckets: Vec<ContextUsageBucketProjection>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ContextUsageBucketProjection {
    pub provider: String,
    pub model: String,
    pub call_purpose: LlmCallPurpose,
    pub call_scope: LlmCallScope,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct McpServerProjection {
    pub name: String,
    pub transport: String,
    pub state: String,
    #[serde(default)]
    pub tool_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TrustProjection {
    #[serde(default)]
    pub mode: TrustMode,
    #[serde(default)]
    pub theme: TrustTheme,
    #[serde(default)]
    pub escalation: TrustEscalation,
    #[serde(default)]
    pub eager_tiers: TrustTierOverrides,
    #[serde(default)]
    pub eager_risks: TrustRiskOverrides,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustMode {
    Calm,
    #[default]
    Steady,
    Eager,
    Reckless,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustTheme {
    #[default]
    Default,
    Wuxia,
    Animal,
    Weather,
    Drink,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustEscalation {
    Deny,
    #[default]
    Ask,
    Allow,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustPolicyAction {
    Auto,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TrustTierOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier0: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier1: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier3: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier4: Option<TrustPolicyAction>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct TrustRiskOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_workspace: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem_write: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_spawn: Option<TrustPolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_mutation: Option<TrustPolicyAction>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct InteractionProjection {
    #[serde(default)]
    pub prompts: Vec<PendingPromptProjection>,
    #[serde(default)]
    pub approvals: Vec<ApprovalRequestProjection>,
    #[serde(default)]
    pub approval_groups: Vec<ApprovalGroupProjection>,
    #[serde(default)]
    pub forms: Vec<PendingFormProjection>,
    #[serde(default)]
    pub compact_reviews: Vec<CompactReviewProjection>,
    #[serde(default)]
    pub interjections: Vec<InterjectionProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct InterjectionProjection {
    pub id: Uuid,
    pub turn_id: TurnId,
    #[serde(default)]
    pub run_id: Option<FlowRunId>,
    pub text: String,
    pub level: InterjectionLevel,
    pub state: InterjectionState,
    #[serde(default)]
    pub redirect_target: Option<String>,
    pub created_at: DateTime<Utc>,
    pub source: InterjectionSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InterjectionSource {
    User,
    Watcher {
        watcher_id: String,
        kind: String,
        handle: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ApprovalRequestProjection {
    pub id: Uuid,
    pub run_id: FlowRunId,
    pub tool_name: String,
    pub tier: u8,
    pub state: ApprovalState,
    #[serde(default)]
    pub target: Option<ApprovalTarget>,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Evaluating,
    Pending,
    Approved,
    Denied,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalTarget {
    Flow { run_id: FlowRunId },
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ApprovalGroupProjection {
    pub id: Uuid,
    pub label: String,
    pub request_ids: Vec<Uuid>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct PendingPromptProjection {
    pub id: crate::PromptId,
    pub kind: String,
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct PendingFormProjection {
    pub id: String,
    pub run_id: FlowRunId,
    pub tool_use_id: String,
    pub emitted_at: DateTime<Utc>,
    pub questions: Vec<FormQuestionProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct FormQuestionProjection {
    pub id: String,
    pub kind: FormQuestionKind,
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub min: Option<usize>,
    #[serde(default)]
    pub max: Option<usize>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub multiline: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FormQuestionKind {
    Confirm,
    SingleSelect,
    MultiSelect,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct CompactReviewProjection {
    pub id: String,
    pub context_id: Option<ContextId>,
    pub summary: String,
    pub slice_preview: String,
    pub slice_count: usize,
    pub range_start: usize,
    pub range_end: usize,
    pub tokens_before: u64,
    pub emitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ResourceProjection {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub state: ResourceState,
    pub owner_run_id: FlowRunId,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Terminal,
    BackgroundProcess,
    Workspace,
    Artifact,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Starting,
    Running,
    Dirty,
    Exited,
    Failed,
    Terminating,
    Retained,
    Released,
    Lost,
    Orphaned,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct UsageProjection {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub llm_calls: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ProjectionDelta {
    pub base_revision: Revision,
    pub revision: Revision,
    pub changes: Vec<ProjectionChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectionChange {
    MetadataSet {
        metadata: SessionMetadataProjection,
    },
    LifecycleSet {
        lifecycle: SessionLifecycle,
    },
    RunUpsert {
        run: RunProjection,
    },
    RunRemove {
        run_id: FlowRunId,
    },
    TranscriptAppend {
        items: Vec<TranscriptItem>,
    },
    TranscriptReplace {
        items: Vec<TranscriptItem>,
    },
    WorkflowsReplace {
        workflows: Vec<WorkflowProjection>,
    },
    CompactionsReplace {
        compactions: Vec<CompactionProjection>,
    },
    GoalSet {
        goal: Option<String>,
    },
    TodosReplace {
        todos: Vec<TodoProjection>,
    },
    PlansReplace {
        plans: Vec<PlanProjection>,
    },
    ContextSet {
        context: ContextProjection,
    },
    TrustSet {
        trust: TrustProjection,
    },
    InteractionsSet {
        interactions: InteractionProjection,
    },
    ResourceUpsert {
        resource: ResourceProjection,
    },
    ResourceRemove {
        resource_id: ResourceId,
    },
    UsageSet {
        usage: UsageProjection,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    ProjectionDelta { delta: ProjectionDelta },
    Signal { signal: SessionSignal },
    ResyncRequired { gap: ResyncRequired },
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionSignal {
    LlmText {
        run_id: FlowRunId,
        text: String,
    },
    Thinking {
        run_id: FlowRunId,
        text: String,
    },
    ToolCallDraft {
        run_id: FlowRunId,
        index: usize,
        call_id: String,
        name: String,
        arguments_delta: String,
    },
    LlmDone {
        run_id: FlowRunId,
        total_tokens: u64,
    },
    LlmRetry {
        run_id: FlowRunId,
    },
    Notification {
        notification: SessionNotification,
    },
    TerminalBytes {
        resource_id: ResourceId,
        bytes: Vec<u8>,
    },
    ProcessLine {
        resource_id: ResourceId,
        stream: String,
        line: String,
    },
    Progress {
        run_id: FlowRunId,
        label: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SessionNotification {
    #[serde(default)]
    pub run_id: Option<FlowRunId>,
    pub level: NoticeLevel,
    pub location: NotificationLocation,
    pub lifecycle: NotificationLifecycle,
    pub stack: NotificationStack,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLocation {
    Inline,
    Toast,
    Status,
    Modal,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationLifecycle {
    Persistent,
    Ttl { duration_ms: u64 },
    Dismissible,
    UntilReplaced,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationStack {
    Append,
    Replace { key: String },
    Dedupe { key: String, window_ms: u64 },
    MergeCount { key: String, window_ms: u64 },
    Coalesce { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ResyncRequired {
    pub requested_after: EventCursor,
    pub available_from: EventCursor,
    pub snapshot_revision: Revision,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct ProjectionEventEnvelope {
    pub schema_version: u32,
    pub daemon_generation: DaemonGeneration,
    pub session_id: SessionId,
    pub cursor: EventCursor,
    pub ts: DateTime<Utc>,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct GetSessionSnapshotRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct GetSessionUpdatesRequest {
    pub session_id: SessionId,
    #[serde(default)]
    pub after_cursor: EventCursor,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, ToSchema)]
pub struct GetSessionUpdatesResponse {
    pub daemon_generation: DaemonGeneration,
    pub events: Vec<ProjectionEventEnvelope>,
    pub next_cursor: EventCursor,
    pub has_more: bool,
    #[serde(default)]
    pub resync_required: Option<ResyncRequired>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(session_id: SessionId) -> SessionProjection {
        SessionProjection {
            revision: Revision(7),
            metadata: SessionMetadataProjection {
                id: session_id,
                title: "Inspect daemon state".into(),
                name_source: NameSource::User,
                project_root: Some("/workspace".into()),
                created_at: None,
                updated_at: None,
            },
            lifecycle: SessionLifecycle::Active,
            runs: Vec::new(),
            transcript: Vec::new(),
            compactions: Vec::new(),
            workflows: Vec::new(),
            goal: Some("Keep every client convergent".into()),
            todos: Vec::new(),
            plans: Vec::new(),
            context: ContextProjection::default(),
            trust: TrustProjection::default(),
            interactions: InteractionProjection::default(),
            resources: Vec::new(),
            usage: UsageProjection::default(),
        }
    }

    #[test]
    fn snapshot_round_trip_preserves_coverage_boundary() {
        let session_id = SessionId(Uuid::now_v7());
        let snapshot = SessionSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            daemon_generation: DaemonGeneration("generation-a".into()),
            cursor: EventCursor(42),
            projection: projection(session_id.clone()),
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        let decoded: SessionSnapshot = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.projection.metadata.id, session_id);
        assert_eq!(decoded.cursor, EventCursor(42));
    }

    #[test]
    fn task_resource_identity_round_trips() {
        let task_id = Uuid::now_v7();
        let resource_id = ResourceId::task(task_id);
        assert_eq!(resource_id.0, format!("task:{task_id}"));
        assert_eq!(resource_id.task_id(), Some(task_id));
        assert_eq!(ResourceId("workspace:test".into()).task_id(), None);
        assert_eq!(ResourceId("task:invalid".into()).task_id(), None);
    }

    #[test]
    fn legacy_projection_without_trust_uses_the_safe_default() {
        let mut encoded = serde_json::to_value(projection(SessionId(Uuid::now_v7()))).unwrap();
        encoded.as_object_mut().unwrap().remove("trust");
        let decoded: SessionProjection = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.trust, TrustProjection::default());
        assert_eq!(decoded.trust.mode, TrustMode::Steady);
        assert_eq!(decoded.trust.escalation, TrustEscalation::Ask);
    }

    #[test]
    fn projection_delta_carries_exact_revision_precondition() {
        let delta = ProjectionDelta {
            base_revision: Revision(7),
            revision: Revision(8),
            changes: vec![ProjectionChange::GoalSet {
                goal: Some("Converged".into()),
            }],
        };
        let encoded = serde_json::to_value(&delta).unwrap();
        assert_eq!(encoded["base_revision"], 7);
        assert_eq!(encoded["revision"], 8);
        assert_eq!(encoded["changes"][0]["type"], "goal_set");
    }

    #[test]
    fn protocol_registry_can_describe_methods_before_a_daemon_advertises_them() {
        let snapshot = crate::method_descriptor::<crate::rpc::GetSessionSnapshot>();
        let updates = crate::method_descriptor::<crate::rpc::GetSessionUpdates>();
        assert_eq!(snapshot.name, crate::methods::GET_SESSION_SNAPSHOT);
        assert_eq!(updates.name, crate::methods::GET_SESSION_UPDATES);
        assert_eq!(snapshot.kind, crate::RpcKind::Query);
        assert_eq!(updates.kind, crate::RpcKind::Query);
    }
}
