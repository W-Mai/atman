// Generated from atman-proto. Do not edit.

export type AtmanDaemonProtocolPayloads =
  | CapabilitiesRequest
  | CapabilitiesResponse
  | EmptyParams
  | PingResponse
  | CreateSessionRequest
  | SessionSnapshot
  | CloseSessionRequest
  | CloseSessionResponse
  | DeleteSessionRequest
  | DeleteSessionResponse
  | SanitizeSessionAttachmentsRequest
  | SanitizeSessionAttachmentsResponse
  | ImportSessionMessagesRequest
  | ImportSessionMessagesResponse
  | MutateProviderRequest
  | ProviderMutationResult
  | InitializeConfigRequest
  | InitializeConfigResponse
  | UpsertModelConfigRequest
  | UpsertModelConfigResponse
  | SwitchDefaultModelRequest
  | SwitchDefaultModelResponse
  | ProbeProviderRequest
  | ProbeResponse
  | McpServerRequest
  | ListMcpServersResponse
  | MutateMcpServerRequest
  | MutateMcpServerResponse
  | ListMcpToolsResponse
  | ListMcpResourcesResponse
  | ListMcpPromptsResponse
  | SendMessageRequest
  | SendMessageResponse
  | InterjectSessionRequest
  | InterjectSessionResponse
  | UpdateSessionTrustRequest
  | UpdateSessionTrustResponse
  | ReloadSessionMcpRequest
  | ReloadSessionMcpResponse
  | AutoNameSessionRequest
  | AutoNameSessionResponse
  | SuggestFlowRequest
  | SuggestFlowResponse
  | InstallSuggestedFlowRequest
  | InstallSuggestedFlowResponse
  | MoveSessionRequest
  | MoveSessionResponse
  | SetSessionGoalRequest
  | SetSessionGoalResponse
  | UpdateSessionTodosRequest
  | UpdateSessionTodosResponse
  | ListProjectsRequest
  | ListProjectsResponse
  | ListSessionsRequest
  | ListSessionsResult
  | RenameSessionRequest
  | RenameSessionResponse
  | StartRunRequest
  | StartRunResponse
  | RunFlowRequest
  | RunFlowResponse
  | CancelRunRequest
  | CancelRunResponse
  | GetEventsRequest
  | GetEventsResponse
  | GetSessionSnapshotRequest
  | GetSessionUpdatesRequest
  | GetSessionUpdatesResponse
  | GetSessionTimelineTailRequest
  | SessionTimelinePage
  | GetSessionTimelineBeforeRequest
  | GetSessionTimelineAfterRequest
  | GetSessionTimelineAroundRequest
  | GetSessionTimelineItemDetailRequest
  | SessionTimelineItemDetail
  | ResolvePromptRequest
  | ResolvePromptResponse
  | SubmitFormRequest
  | SubmitFormResponse
  | CompactSessionRequest
  | CompactSessionResponse
  | ResolveCompactReviewRequest
  | ResolveCompactReviewResponse
  | ListPermissionRequestsRequest
  | ListPermissionRequestsResponse
  | CreatePermissionGroupRequest
  | CreatePermissionGroupResponse
  | ResolvePermissionRequestsRequest
  | ResolvePermissionRequestsResponse
  | ListResourcesRequest
  | ListResourcesResponse
  | InspectResourceRequest
  | InspectResourceResponse
  | TerminateResourceRequest
  | TerminateResourceResponse
  | ResizeTerminalResourceRequest
  | ResizeTerminalResourceResponse
  | RetainResourceRequest
  | RetainResourceResponse
  | ReleaseResourceRequest
  | ReleaseResourceResponse
  | JsonRpcRequest
  | JsonRpcResponse
  | JsonRpcError
  | ServerEventEnvelope
  | ProjectionEventEnvelope
export type ClientId = string
export type DaemonGeneration = string
export type RpcKind = 'command' | 'query'
export type RequestId = string
export type EventCursor = number
export type ContextId = string
export type CompactionOperationId = string
export type FlowRunId = string
export type McpServerStateProjection =
  | {
      type: 'disabled'
      [k: string]: unknown
    }
  | {
      type: 'pending'
      [k: string]: unknown
    }
  | {
      type: 'connecting'
      [k: string]: unknown
    }
  | {
      tools?: McpToolProjection[]
      type: 'connected'
      [k: string]: unknown
    }
  | {
      message: string
      type: 'error'
      [k: string]: unknown
    }
  | {
      message: string
      type: 'disconnected'
      [k: string]: unknown
    }
  | {
      message: string
      type: 'timeout'
      [k: string]: unknown
    }
export type McpTransportProjection = 'stdio' | 'http' | 'sse'
export type LlmCallPurpose =
  | 'general'
  | 'classification'
  | 'extraction'
  | 'branch_generation'
  | 'compaction'
  | 'interjection_classification'
export type LlmCallScope = 'root' | 'child' | 'detached'
export type ApprovalGroupOwnerProjection =
  | {
      run_id: FlowRunId
      type: 'flow'
      [k: string]: unknown
    }
  | {
      session_id: string
      type: 'user'
      [k: string]: unknown
    }
  | {
      type: 'system'
      [k: string]: unknown
    }
export type ApprovalActorProjection =
  | {
      policy_version: string
      rule_id: string
      type: 'policy'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      session_id: string
      type: 'flow'
      [k: string]: unknown
    }
  | {
      principal_id?: string | null
      session_id: string
      type: 'user'
      [k: string]: unknown
    }
  | {
      component: string
      type: 'system'
      [k: string]: unknown
    }
  | {
      label: string
      type: 'unknown_legacy'
      [k: string]: unknown
    }
export type ApprovalTarget =
  | {
      run_id: FlowRunId
      type: 'flow'
      [k: string]: unknown
    }
  | {
      type: 'user'
      [k: string]: unknown
    }
export type ApprovalExecutionBoundary = 'sandboxed' | 'direct'
export type ApprovalScopeProjection =
  | {
      type: 'current_call'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      tool_name: string
      type: 'child_run_same_tool'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      tool_name: string
      type: 'child_run_same_path_rule'
      workspace_relative_path: string
      [k: string]: unknown
    }
export type ApprovalState = 'evaluating' | 'pending' | 'approved' | 'denied' | 'cancelled'
export type FormQuestionKind = 'confirm' | 'single_select' | 'multi_select' | 'text'
export type InterjectionLevel = 'nudge' | 'course_correct' | 'redirect' | 'hard_stop'
export type InterjectionSource =
  | {
      type: 'user'
      [k: string]: unknown
    }
  | {
      handle: string
      kind: string
      type: 'watcher'
      watcher_id: string
      [k: string]: unknown
    }
export type InterjectionState = 'pending' | 'injected' | 'cancelled'
export type TurnId = string
export type PromptId = string
export type SessionLifecycle = 'creating' | 'idle' | 'active' | 'degraded' | 'closing' | 'closed'
export type SessionId = string
export type NameSource = 'auto' | 'user'
export type ResourceId = string
export type ResourceKind = 'terminal' | 'background_process' | 'workspace' | 'artifact'
export type ResourceState =
  | 'starting'
  | 'running'
  | 'dirty'
  | 'exited'
  | 'failed'
  | 'terminating'
  | 'retained'
  | 'released'
  | 'lost'
  | 'orphaned'
export type Revision = number
export type RunLifecycle =
  | 'queued'
  | 'starting'
  | 'running'
  | 'waiting_input'
  | 'cancelling'
  | 'cancelled'
  | 'succeeded'
  | 'failed'
  | 'lost'
export type TodoState = 'pending' | 'in_progress' | 'done' | 'cancelled'
export type TranscriptItem =
  | {
      checkpoint_index?: number | null
      context_id?: null | ContextId
      message: MessageProjection
      run_id?: null | FlowRunId
      seq: number
      ts: string
      type: 'message'
      [k: string]: unknown
    }
  | {
      new_content?: string | null
      old_content?: string | null
      run_id?: null | FlowRunId
      seq: number
      title: string
      tool_use_id?: string | null
      ts: string
      type: 'diff'
      unified_diff?: string | null
      [k: string]: unknown
    }
  | {
      added_lines: number
      hunks: number
      path: string
      removed_lines: number
      run_id?: null | FlowRunId
      seq: number
      tool_name: string
      tool_use_id?: string | null
      ts: string
      turn_id?: null | TurnId
      type: 'file_edit'
      [k: string]: unknown
    }
  | {
      seq: number
      session: ActivityTotalsProjection
      session_files: string[]
      ts: string
      turn: ActivityTotalsProjection
      turn_files: string[]
      turn_id: TurnId
      type: 'activity_summary'
      [k: string]: unknown
    }
  | {
      after_tokens: number
      before_tokens: number
      compacted_count: number
      context_id?: null | ContextId
      operation_id?: null | CompactionOperationId
      outcome?: CompactionOutcome
      range_end: number
      range_start: number
      run_id?: null | FlowRunId
      seq: number
      summary: string
      ts: string
      type: 'compaction'
      [k: string]: unknown
    }
  | {
      seq: number
      source: string
      ts: string
      type: 'mermaid'
      [k: string]: unknown
    }
  | {
      level: NoticeLevel
      seq: number
      text: string
      ts: string
      type: 'notice'
      [k: string]: unknown
    }
  | {
      kind: string
      payload: {
        [k: string]: unknown
      }
      seq: number
      ts: string
      type: 'extension'
      [k: string]: unknown
    }
export type MessageOrigin = 'user' | 'watcher' | 'interjection' | 'internal'
export type MessagePart =
  | {
      content: string
      digest: string
      key: string
      revision: number
      type: 'context_record'
      [k: string]: unknown
    }
  | {
      count: number
      seq_end: number
      seq_start: number
      summary: string
      type: 'compact_summary'
      [k: string]: unknown
    }
  | {
      text: string
      type: 'text'
      [k: string]: unknown
    }
  | {
      thinking: string
      type: 'thinking'
      [k: string]: unknown
    }
  | {
      artifact_id?: string | null
      detail: ImageDetail
      id?: null | MessagePartId
      media_type: string
      name?: string | null
      type: 'image'
      [k: string]: unknown
    }
  | {
      id: string
      input: {
        [k: string]: unknown
      }
      intent?: string | null
      name: string
      type: 'tool_use'
      [k: string]: unknown
    }
  | {
      content: string
      is_error?: boolean
      tool_use_id: string
      type: 'tool_result'
      [k: string]: unknown
    }
export type ImageDetail = 'low' | 'high' | 'original' | 'auto'
export type MessagePartId = string
export type MessageRole = 'user' | 'assistant' | 'system' | 'tool'
export type CompactionOutcome = 'finished' | 'failed' | 'abandoned'
export type NoticeLevel = 'debug' | 'info' | 'success' | 'warning' | 'error'
export type TrustPolicyAction = 'auto' | 'ask' | 'deny'
export type TrustEscalation = 'deny' | 'ask' | 'allow'
export type TrustMode = 'calm' | 'steady' | 'eager' | 'reckless'
export type TrustTheme = 'default' | 'wuxia' | 'animal' | 'weather' | 'drink'
export type ApprovalProjection =
  | {
      level: string
      preview?: string | null
      state: 'pending'
      [k: string]: unknown
    }
  | {
      state: 'approved'
      [k: string]: unknown
    }
  | {
      reason: string
      state: 'denied'
      [k: string]: unknown
    }
export type WorkflowNodeKind =
  | {
      flow_name: string
      run_id: FlowRunId
      type: 'flow'
      [k: string]: unknown
    }
  | {
      kind: WorkflowStatementKind
      type: 'statement'
      [k: string]: unknown
    }
  | {
      args_preview: string
      intent?: string | null
      result_preview?: string | null
      tool_name: string
      tool_use_id: string
      type: 'tool_call'
      [k: string]: unknown
    }
  | {
      flow_name: string
      run_id: FlowRunId
      type: 'subflow'
      [k: string]: unknown
    }
  | {
      branch_index: number
      type: 'fanout_branch'
      [k: string]: unknown
    }
export type WorkflowStatementKind =
  | {
      model?: string | null
      type: 'llm'
      [k: string]: unknown
    }
  | {
      path: string
      type: 'tool_call'
      [k: string]: unknown
    }
  | {
      collect: WorkflowFanoutMode
      type: 'fanout'
      [k: string]: unknown
    }
  | {
      type: 'user_confirm'
      [k: string]: unknown
    }
  | {
      name: string
      type: 'subflow'
      [k: string]: unknown
    }
  | {
      role: string
      type: 'message'
      [k: string]: unknown
    }
  | {
      type: 'fix_until_test'
      [k: string]: unknown
    }
  | {
      condition_preview: string
      type: 'when'
      [k: string]: unknown
    }
  | {
      type: 'loop'
      [k: string]: unknown
    }
  | {
      type: 'return'
      [k: string]: unknown
    }
export type WorkflowFanoutMode = 'all' | 'first'
export type WorkflowNodeState = 'pending' | 'running' | 'succeeded' | 'failed' | 'cancelled'
export type SessionCloseStatus = 'closed' | 'already_closed' | 'busy'
export type SessionDeleteStatus = 'deleted' | 'not_found' | 'busy' | 'unsafe_resources'
export type ProviderMutation =
  | {
      kind: ProviderKind
      name: string
      type: 'login'
      [k: string]: unknown
    }
  | {
      enabled: boolean
      provider_id: string
      type: 'set_enabled'
      [k: string]: unknown
    }
  | {
      provider_id: string
      type: 'remove'
      [k: string]: unknown
    }
  | {
      provider_id: string
      type: 'refresh'
      [k: string]: unknown
    }
  | {
      api_key: string
      api_key_env: string
      base_url: string
      create: boolean
      enabled: boolean
      kind: string
      max_tokens?: number | null
      name: string
      reasoning_format: string
      type: 'upsert_config'
      [k: string]: unknown
    }
export type ProviderKind = 'codex' | 'anthropic-oauth' | 'git-hub-copilot' | 'custom'
export type ProviderMutationResult =
  | {
      delta: CatalogDelta
      kind: ProviderKind
      name: string
      provider_id: string
      type: 'installed'
      [k: string]: unknown
    }
  | {
      catalog?: null | CatalogDelta
      change: ProviderStateChange
      enabled?: boolean | null
      provider_id: string
      type: 'state_changed'
      [k: string]: unknown
    }
  | {
      delta: CatalogDelta
      provider_id: string
      type: 'refreshed'
      [k: string]: unknown
    }
  | {
      created: boolean
      name: string
      type: 'config_saved'
      [k: string]: unknown
    }
export type McpServerMutation =
  | {
      action: 'upsert'
      server: McpServerInput
      [k: string]: unknown
    }
  | {
      action: 'remove'
      name: string
      [k: string]: unknown
    }
export type SessionStatus = 'running' | 'finished' | 'pending'
export type AutoNameSessionStatus = 'updated' | 'superseded'
export type SuggestFlowStatus =
  | {
      status: 'no_suggestion'
      [k: string]: unknown
    }
  | {
      reason: string
      status: 'invalid'
      [k: string]: unknown
    }
  | {
      flow_name: string
      has_shell: boolean
      source: string
      status: 'proposal'
      [k: string]: unknown
    }
export type TodoMutation =
  | {
      action: 'clear'
      [k: string]: unknown
    }
  | {
      action: 'set_state'
      id: string
      state: TodoState
      [k: string]: unknown
    }
export type ProjectId = string
export type ListSessionsResult = {
  event_count: number
  first_ts?: string | null
  goal?: string | null
  id: SessionId
  message_count: number
  name_source?: NameSource
  project_root?: string | null
  status: SessionStatus
  title?: string
  updated_at?: string | null
  [k: string]: unknown
}[]
export type RunCancellationStatus = 'accepted' | 'already_requested' | 'not_found'
export type ServerEvent =
  | {
      delta: ProjectionDelta
      type: 'projection_delta'
      [k: string]: unknown
    }
  | {
      signal: SessionSignal
      type: 'signal'
      [k: string]: unknown
    }
  | {
      gap: ResyncRequired
      type: 'resync_required'
      [k: string]: unknown
    }
  | {
      type: 'heartbeat'
      [k: string]: unknown
    }
export type ProjectionChange =
  | {
      metadata: SessionMetadataProjection
      type: 'metadata_set'
      [k: string]: unknown
    }
  | {
      lifecycle: SessionLifecycle
      type: 'lifecycle_set'
      [k: string]: unknown
    }
  | {
      run: RunProjection
      type: 'run_upsert'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      type: 'run_remove'
      [k: string]: unknown
    }
  | {
      items: TranscriptItem[]
      type: 'transcript_append'
      [k: string]: unknown
    }
  | {
      items: TranscriptItem[]
      type: 'transcript_replace'
      [k: string]: unknown
    }
  | {
      type: 'workflow_upsert'
      workflow: WorkflowProjection
      [k: string]: unknown
    }
  | {
      turn_id: TurnId
      type: 'workflow_remove'
      [k: string]: unknown
    }
  | {
      compactions: CompactionProjection[]
      type: 'compactions_replace'
      [k: string]: unknown
    }
  | {
      goal?: string | null
      type: 'goal_set'
      [k: string]: unknown
    }
  | {
      todos: TodoProjection[]
      type: 'todos_replace'
      [k: string]: unknown
    }
  | {
      plans: PlanProjection[]
      type: 'plans_replace'
      [k: string]: unknown
    }
  | {
      context: ContextProjection
      type: 'context_set'
      [k: string]: unknown
    }
  | {
      trust: TrustProjection
      type: 'trust_set'
      [k: string]: unknown
    }
  | {
      interaction: InteractionItem
      type: 'interaction_upsert'
      [k: string]: unknown
    }
  | {
      target: InteractionTarget
      type: 'interaction_remove'
      [k: string]: unknown
    }
  | {
      resource: ResourceProjection
      type: 'resource_upsert'
      [k: string]: unknown
    }
  | {
      resource_id: ResourceId
      type: 'resource_remove'
      [k: string]: unknown
    }
  | {
      type: 'usage_set'
      usage: UsageProjection
      [k: string]: unknown
    }
export type InteractionItem =
  | {
      prompt: PendingPromptProjection
      type: 'prompt'
      [k: string]: unknown
    }
  | {
      approval: ApprovalRequestProjection
      type: 'approval'
      [k: string]: unknown
    }
  | {
      group: ApprovalGroupProjection
      type: 'approval_group'
      [k: string]: unknown
    }
  | {
      form: PendingFormProjection
      type: 'form'
      [k: string]: unknown
    }
  | {
      review: CompactReviewProjection
      type: 'compact_review'
      [k: string]: unknown
    }
  | {
      interjection: InterjectionProjection
      type: 'interjection'
      [k: string]: unknown
    }
export type InteractionTarget =
  | {
      prompt_id: PromptId
      type: 'prompt'
      [k: string]: unknown
    }
  | {
      approval_id: string
      type: 'approval'
      [k: string]: unknown
    }
  | {
      group_id: string
      type: 'approval_group'
      [k: string]: unknown
    }
  | {
      form_id: string
      type: 'form'
      [k: string]: unknown
    }
  | {
      review_id: string
      type: 'compact_review'
      [k: string]: unknown
    }
  | {
      interjection_id: string
      type: 'interjection'
      [k: string]: unknown
    }
export type SessionSignal =
  | {
      run_id: FlowRunId
      text: string
      type: 'llm_text'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      text: string
      type: 'thinking'
      [k: string]: unknown
    }
  | {
      arguments_delta: string
      call_id: string
      index: number
      name: string
      run_id: FlowRunId
      type: 'tool_call_draft'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      total_tokens: number
      type: 'llm_done'
      [k: string]: unknown
    }
  | {
      run_id: FlowRunId
      type: 'llm_retry'
      [k: string]: unknown
    }
  | {
      notification: SessionNotification
      type: 'notification'
      [k: string]: unknown
    }
  | {
      bytes: number[]
      resource_id: ResourceId
      type: 'terminal_bytes'
      [k: string]: unknown
    }
  | {
      line: string
      resource_id: ResourceId
      stream: string
      type: 'process_line'
      [k: string]: unknown
    }
  | {
      label: string
      run_id: FlowRunId
      type: 'progress'
      [k: string]: unknown
    }
export type NotificationLifecycle =
  | {
      type: 'persistent'
      [k: string]: unknown
    }
  | {
      duration_ms: number
      type: 'ttl'
      [k: string]: unknown
    }
  | {
      type: 'dismissible'
      [k: string]: unknown
    }
  | {
      type: 'until_replaced'
      [k: string]: unknown
    }
export type NotificationLocation = 'inline' | 'toast' | 'status' | 'modal' | 'stdout' | 'stderr'
export type NotificationStack =
  | {
      type: 'append'
      [k: string]: unknown
    }
  | {
      key: string
      type: 'replace'
      [k: string]: unknown
    }
  | {
      key: string
      type: 'dedupe'
      window_ms: number
      [k: string]: unknown
    }
  | {
      key: string
      type: 'merge_count'
      window_ms: number
      [k: string]: unknown
    }
  | {
      key: string
      type: 'coalesce'
      [k: string]: unknown
    }
export type GetSessionTimelineTailRequest = SessionTimelineBudget & {
  session_id: SessionId
  [k: string]: unknown
}
export type TimelineSegment =
  | {
      segment: TimelineTurnSegment
      type: 'turn'
      [k: string]: unknown
    }
  | {
      segment: TimelineSessionSegment
      type: 'session'
      [k: string]: unknown
    }
export type TimelineSegmentId = string
export type TimelineItemId = string
export type TimelineItemKind =
  | 'message'
  | 'diff'
  | 'file_edit'
  | 'activity_summary'
  | 'compaction'
  | 'mermaid'
  | 'notice'
  | 'extension'
export type TimelineTurnState = 'active' | 'complete'
export type GetSessionTimelineBeforeRequest = SessionTimelineBudget & {
  before: TimelineCursor
  session_id: SessionId
  [k: string]: unknown
}
export type GetSessionTimelineAfterRequest = SessionTimelineBudget & {
  after: TimelineCursor
  session_id: SessionId
  [k: string]: unknown
}
export type GetSessionTimelineAroundRequest = SessionTimelineBudget & {
  anchor: TimelineItemId
  session_id: SessionId
  [k: string]: unknown
}
export type PromptResolutionStatus = 'resolved' | 'already_resolved' | 'abandoned' | 'not_found'
export type FormSubmission =
  | {
      answers: FormAnswer[]
      status: 'submitted'
      [k: string]: unknown
    }
  | {
      status: 'rejected'
      [k: string]: unknown
    }
export type FormAnswer =
  | {
      kind: 'confirmed'
      value: boolean
      [k: string]: unknown
    }
  | {
      index: number
      kind: 'selected'
      label: string
      [k: string]: unknown
    }
  | {
      indices: number[]
      kind: 'multi_selected'
      labels: string[]
      [k: string]: unknown
    }
  | {
      kind: 'text_entered'
      text: string
      [k: string]: unknown
    }
  | {
      kind: 'cancelled'
      [k: string]: unknown
    }
export type FormResolutionStatus = 'resolved' | 'already_resolved' | 'abandoned' | 'not_found'
export type CompactionRequestStatus = 'accepted' | 'already_running'
export type CompactReviewDecision =
  | {
      decision: 'accept_as_is'
      [k: string]: unknown
    }
  | {
      decision: 'accept_edited'
      summary: string
      [k: string]: unknown
    }
  | {
      decision: 'reject'
      [k: string]: unknown
    }
export type CompactReviewResolutionStatus =
  'resolved' | 'already_resolved' | 'abandoned' | 'not_found'
export type PermissionRpcAction = 'approve' | 'deny' | 'defer'
export type PermissionRpcScope =
  | 'current_call'
  | {
      child_run_same_tool: {
        run_id: FlowRunId
        tool_name: string
        [k: string]: unknown
      }
      [k: string]: unknown
    }
  | {
      child_run_same_path_rule: {
        run_id: FlowRunId
        tool_name: string
        workspace_relative_path: string
        [k: string]: unknown
      }
      [k: string]: unknown
    }
export type PermissionRpcSelector =
  | {
      expected_request_revisions: {
        [k: string]: number
      }
      request_ids: string[]
      [k: string]: unknown
    }
  | {
      expected_group_revision: number
      group_id: string
      [k: string]: unknown
    }
export type ResourceTerminationStatus =
  'terminating' | 'already_terminal' | 'unavailable' | 'unsupported' | 'not_found'
export type TerminalResizeStatus =
  'resized' | 'already_terminal' | 'unavailable' | 'unsupported' | 'not_found'

export interface CapabilitiesRequest {
  client_id?: null | ClientId
  client_name?: string | null
  client_version?: string | null
  protocol_version?: number | null
  [k: string]: unknown
}
export interface CapabilitiesResponse {
  daemon_generation: DaemonGeneration
  daemon_version: string
  event_schema_version: number
  limits: ProtocolLimits
  methods: MethodCapability[]
  protocol_version: number
  snapshot_schema_version?: number
  [k: string]: unknown
}
export interface ProtocolLimits {
  max_event_page_size: number
  subscriber_buffer: number
  [k: string]: unknown
}
export interface MethodCapability {
  kind: RpcKind
  name: string
  revision: number
  [k: string]: unknown
}
export interface EmptyParams {
  [k: string]: unknown
}
export interface PingResponse {
  pong: boolean
  version: string
  [k: string]: unknown
}
export interface CreateSessionRequest {
  project_root?: string | null
  request_id?: null | RequestId
  title?: string | null
  [k: string]: unknown
}
export interface SessionSnapshot {
  cursor: EventCursor
  daemon_generation: DaemonGeneration
  projection: SessionProjection
  schema_version: number
  [k: string]: unknown
}
export interface SessionProjection {
  compactions?: CompactionProjection[]
  context?: ContextProjection
  goal?: string | null
  interactions?: InteractionProjection
  lifecycle: SessionLifecycle
  metadata: SessionMetadataProjection
  plans?: PlanProjection[]
  resources?: ResourceProjection[]
  revision: Revision
  runs?: RunProjection[]
  todos?: TodoProjection[]
  transcript?: TranscriptItem[]
  trust?: TrustProjection
  usage?: UsageProjection
  workflows?: WorkflowProjection[]
  [k: string]: unknown
}
export interface CompactionProjection {
  before_tokens: number
  compacted_count: number
  context_id?: null | ContextId
  id: CompactionOperationId
  range_end: number
  range_start: number
  run_id?: null | FlowRunId
  started_at: string
  started_seq: number
  summary?: string
  [k: string]: unknown
}
export interface ContextProjection {
  cache_read_tokens?: number
  cache_write_tokens?: number
  cost_usd?: number
  input_tokens?: number
  last_tokens_per_second?: number
  last_ttft_ms?: number
  mcp_servers?: McpServerProjection[]
  memory_recent_count?: number
  model?: string
  output_tokens?: number
  provider?: string
  usage_buckets?: ContextUsageBucketProjection[]
  window_budget?: number
  window_tokens?: number
  [k: string]: unknown
}
export interface McpServerProjection {
  name: string
  state: McpServerStateProjection
  transport: McpTransportProjection
  [k: string]: unknown
}
export interface McpToolProjection {
  description?: string | null
  name: string
  [k: string]: unknown
}
export interface ContextUsageBucketProjection {
  cache_read_tokens: number
  cache_write_tokens: number
  call_purpose: LlmCallPurpose
  call_scope: LlmCallScope
  calls: number
  input_tokens: number
  model: string
  output_tokens: number
  provider: string
  [k: string]: unknown
}
export interface InteractionProjection {
  approval_groups?: ApprovalGroupProjection[]
  approvals?: ApprovalRequestProjection[]
  compact_reviews?: CompactReviewProjection[]
  forms?: PendingFormProjection[]
  interjections?: InterjectionProjection[]
  prompts?: PendingPromptProjection[]
  [k: string]: unknown
}
export interface ApprovalGroupProjection {
  at: string
  id: string
  label: string
  owner: ApprovalGroupOwnerProjection
  request_ids: string[]
  resolved: boolean
  revision: number
  [k: string]: unknown
}
export interface ApprovalRequestProjection {
  actor?: null | ApprovalActorProjection
  at: string
  decision_id?: string | null
  escalation_path?: ApprovalEscalationHopProjection[]
  execution_boundary?: null | ApprovalExecutionBoundary
  group_ids?: string[]
  id: string
  intent?: string | null
  parent_run_id?: null | FlowRunId
  policy: ApprovalPolicyProjection
  provenance: ApprovalProvenanceProjection
  reason?: string | null
  requesting_run_id: FlowRunId
  revision: number
  root_run_id: FlowRunId
  scope?: null | ApprovalScopeProjection
  session_id: string
  state: ApprovalState
  target?: null | ApprovalTarget
  tier: number
  tool_name: string
  tool_use_id: string
  [k: string]: unknown
}
export interface ApprovalEscalationHopProjection {
  action?: string | null
  actor?: null | ApprovalActorProjection
  at: string
  reason?: string | null
  target: ApprovalTarget
  [k: string]: unknown
}
export interface ApprovalPolicyProjection {
  rule_id: string
  snapshot_id: string
  [k: string]: unknown
}
export interface ApprovalProvenanceProjection {
  cwd?: string | null
  network?: boolean
  path?: string | null
  path_origin?: string | null
  repository_root?: string | null
  risks?: string[]
  targets?: string[]
  workspace_id?: string | null
  workspace_root?: string | null
  [k: string]: unknown
}
export interface CompactReviewProjection {
  context_id?: null | ContextId
  emitted_at: string
  id: string
  range_end: number
  range_start: number
  slice_count: number
  slice_preview: string
  summary: string
  tokens_before: number
  [k: string]: unknown
}
export interface PendingFormProjection {
  emitted_at: string
  id: string
  questions: FormQuestionProjection[]
  run_id: FlowRunId
  tool_use_id: string
  [k: string]: unknown
}
export interface FormQuestionProjection {
  id: string
  kind: FormQuestionKind
  max?: number | null
  min?: number | null
  multiline?: boolean
  options?: string[]
  placeholder?: string | null
  prompt: string
  [k: string]: unknown
}
export interface InterjectionProjection {
  created_at: string
  id: string
  level: InterjectionLevel
  redirect_target?: string | null
  run_id?: null | FlowRunId
  source: InterjectionSource
  state: InterjectionState
  text: string
  turn_id: TurnId
  [k: string]: unknown
}
export interface PendingPromptProjection {
  id: PromptId
  kind: string
  payload: {
    [k: string]: unknown
  }
  [k: string]: unknown
}
export interface SessionMetadataProjection {
  created_at?: string | null
  id: SessionId
  name_source?: NameSource
  project_root?: string | null
  title?: string
  updated_at?: string | null
  [k: string]: unknown
}
export interface PlanProjection {
  created_at: string
  id: string
  steps: PlanStepProjection[]
  title: string
  updated_at: string
  [k: string]: unknown
}
export interface PlanStepProjection {
  done?: boolean
  done_at?: string | null
  index: number
  text: string
  [k: string]: unknown
}
export interface ResourceProjection {
  details?: {
    [k: string]: string
  }
  finished_at?: string | null
  id: ResourceId
  kind: ResourceKind
  label?: string
  owner_run_id: FlowRunId
  started_at?: string | null
  state: ResourceState
  tool_use_id?: string | null
  [k: string]: unknown
}
export interface RunProjection {
  error?: string | null
  finished_at?: string | null
  flow_name?: string
  id: FlowRunId
  model?: string | null
  output?: string | null
  parent_node_id?: string | null
  parent_run_id?: null | FlowRunId
  provider?: string | null
  started_at: string
  state: RunLifecycle
  turn_id?: null | string
  [k: string]: unknown
}
export interface TodoProjection {
  expected_result: string
  how: string
  id: string
  state: TodoState
  where: string
  why: string
  [k: string]: unknown
}
export interface MessageProjection {
  origin: MessageOrigin
  parts: MessagePart[]
  role: MessageRole
  turn_id: TurnId
  [k: string]: unknown
}
export interface ActivityTotalsProjection {
  applied_edits: number
  attempted_calls: number
  completed_calls: number
  deletions: number
  failed_calls: number
  files: number
  hunks: number
  insertions: number
  [k: string]: unknown
}
export interface TrustProjection {
  eager_risks?: TrustRiskOverrides
  eager_tiers?: TrustTierOverrides
  escalation?: TrustEscalation
  mode?: TrustMode
  theme?: TrustTheme
  [k: string]: unknown
}
export interface TrustRiskOverrides {
  filesystem_write?: null | TrustPolicyAction
  irreversible?: null | TrustPolicyAction
  network?: null | TrustPolicyAction
  outside_workspace?: null | TrustPolicyAction
  process_spawn?: null | TrustPolicyAction
  repository_mutation?: null | TrustPolicyAction
  [k: string]: unknown
}
export interface TrustTierOverrides {
  tier0?: null | TrustPolicyAction
  tier1?: null | TrustPolicyAction
  tier2?: null | TrustPolicyAction
  tier3?: null | TrustPolicyAction
  tier4?: null | TrustPolicyAction
  [k: string]: unknown
}
export interface UsageProjection {
  cache_read_tokens?: number
  cache_write_tokens?: number
  cost_usd?: number
  input_tokens?: number
  llm_calls?: number
  output_tokens?: number
  [k: string]: unknown
}
export interface WorkflowProjection {
  roots?: WorkflowNodeProjection[]
  turn_id: TurnId
  [k: string]: unknown
}
export interface WorkflowNodeProjection {
  approval?: null | ApprovalProjection
  children?: WorkflowNodeProjection[]
  finished_at?: string | null
  id: string
  kind: WorkflowNodeKind
  label: string
  llm_usage?: null | LlmUsageProjection
  output_preview?: string | null
  parallel?: boolean
  started_at?: string | null
  state: WorkflowNodeState
  [k: string]: unknown
}
export interface LlmUsageProjection {
  cache_read_tokens?: number
  cache_write_tokens?: number
  call_purpose?: LlmCallPurpose
  call_scope?: LlmCallScope
  input_tokens?: number
  model?: string
  output_tokens?: number
  provider?: string
  tokens_per_second?: number
  ttft_ms?: number
  wallclock_ms?: number
  [k: string]: unknown
}
export interface CloseSessionRequest {
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface CloseSessionResponse {
  session_id: SessionId
  status: SessionCloseStatus
  [k: string]: unknown
}
export interface DeleteSessionRequest {
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface DeleteSessionResponse {
  blocking_resources?: ResourceId[]
  session_id: SessionId
  status: SessionDeleteStatus
  [k: string]: unknown
}
export interface SanitizeSessionAttachmentsRequest {
  dry_run?: boolean
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface SanitizeSessionAttachmentsResponse {
  cursor: EventCursor
  issues: AttachmentIssue[]
  repaired: number
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface AttachmentIssue {
  context: string
  file_basename: string
  part_id: string
  reason: string
  [k: string]: unknown
}
export interface ImportSessionMessagesRequest {
  messages: ImportedMessage[]
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface ImportedMessage {
  role: MessageRole
  text: string
  [k: string]: unknown
}
export interface ImportSessionMessagesResponse {
  cursor: EventCursor
  imported: number
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface MutateProviderRequest {
  mutation: ProviderMutation
  request_id?: null | RequestId
  [k: string]: unknown
}
export interface CatalogDelta {
  added: number
  removed: number
  total: number
  updated: number
  [k: string]: unknown
}
export interface ProviderStateChange {
  auth_changed: boolean
  catalog_changed: boolean
  live_changed: boolean
  [k: string]: unknown
}
export interface InitializeConfigRequest {
  fs_access?: string | null
  request_id?: null | RequestId
  [k: string]: unknown
}
export interface InitializeConfigResponse {
  config_dir: string
  managed: string[]
  skipped: string[]
  written: string[]
  [k: string]: unknown
}
export interface UpsertModelConfigRequest {
  context_budget: number
  enabled: boolean
  max_tokens?: number | null
  model: string
  name: string
  old_name?: string | null
  provider?: string | null
  reasoning: string
  request_id?: null | RequestId
  [k: string]: unknown
}
export interface UpsertModelConfigResponse {
  name: string
  [k: string]: unknown
}
export interface SwitchDefaultModelRequest {
  model: string
  request_id?: null | RequestId
  [k: string]: unknown
}
export interface SwitchDefaultModelResponse {
  model: string
  [k: string]: unknown
}
export interface ProbeProviderRequest {
  api_key?: string | null
  api_key_env?: string | null
  base_url?: string | null
  enabled: boolean
  kind: string
  max_tokens?: number | null
  name: string
  prompt_cache_key?: boolean | null
  reasoning_format?: string | null
  [k: string]: unknown
}
export interface ProbeResponse {
  message: string
  ok: boolean
  [k: string]: unknown
}
export interface McpServerRequest {
  name: string
  [k: string]: unknown
}
export interface ListMcpServersResponse {
  servers: McpServerSummary[]
  [k: string]: unknown
}
export interface McpServerSummary {
  args: string[]
  command: string
  disabled: boolean
  env_count: number
  header_count: number
  name: string
  tier: number
  timeout_ms: number
  transport: string
  url?: string | null
  [k: string]: unknown
}
export interface MutateMcpServerRequest {
  mutation: McpServerMutation
  request_id?: null | RequestId
  [k: string]: unknown
}
export interface McpServerInput {
  args?: string[]
  auth_token?: string | null
  command?: string
  disabled?: boolean
  env?: McpKeyValue[]
  headers?: McpKeyValue[]
  name: string
  tier: number
  timeout_ms: number
  transport: string
  url?: string | null
  [k: string]: unknown
}
export interface McpKeyValue {
  name: string
  value: string
  [k: string]: unknown
}
export interface MutateMcpServerResponse {
  name: string
  removed: boolean
  [k: string]: unknown
}
export interface ListMcpToolsResponse {
  tools: McpTool[]
  [k: string]: unknown
}
export interface McpTool {
  description?: string | null
  name: string
  [k: string]: unknown
}
export interface ListMcpResourcesResponse {
  resources: McpResource[]
  [k: string]: unknown
}
export interface McpResource {
  description?: string | null
  mime_type?: string | null
  name: string
  uri: string
  [k: string]: unknown
}
export interface ListMcpPromptsResponse {
  prompts: McpPrompt[]
  [k: string]: unknown
}
export interface McpPrompt {
  arguments: McpPromptArg[]
  description?: string | null
  name: string
  [k: string]: unknown
}
export interface McpPromptArg {
  description?: string | null
  name: string
  required: boolean
  [k: string]: unknown
}
export interface SendMessageRequest {
  images?: InlineImage[]
  reasoning?: string | null
  request_id?: null | RequestId
  session_id: SessionId
  text: string
  [k: string]: unknown
}
export interface InlineImage {
  data_base64: string
  name?: string | null
  [k: string]: unknown
}
export interface SendMessageResponse {
  cursor: EventCursor
  revision: Revision
  run_id: FlowRunId
  session_id: SessionId
  [k: string]: unknown
}
export interface InterjectSessionRequest {
  level: InterjectionLevel
  redirect_target?: string | null
  request_id?: null | RequestId
  run_id: FlowRunId
  session_id: SessionId
  text: string
  [k: string]: unknown
}
export interface InterjectSessionResponse {
  cursor: EventCursor
  injection_id: string
  revision: Revision
  run_id: FlowRunId
  session_id: SessionId
  state: InterjectionState
  [k: string]: unknown
}
export interface UpdateSessionTrustRequest {
  request_id?: null | RequestId
  session_id: SessionId
  trust: TrustProjection
  [k: string]: unknown
}
export interface UpdateSessionTrustResponse {
  cursor: EventCursor
  revision: Revision
  session_id: SessionId
  trust: TrustProjection
  [k: string]: unknown
}
export interface ReloadSessionMcpRequest {
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface ReloadSessionMcpResponse {
  active_runs: number
  cursor: EventCursor
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface AutoNameSessionRequest {
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface AutoNameSessionResponse {
  cursor: EventCursor
  revision: Revision
  session: SessionSummary
  status: AutoNameSessionStatus
  [k: string]: unknown
}
export interface SessionSummary {
  event_count: number
  first_ts?: string | null
  goal?: string | null
  id: SessionId
  message_count: number
  name_source?: NameSource
  project_root?: string | null
  status: SessionStatus
  title?: string
  updated_at?: string | null
  [k: string]: unknown
}
export interface SuggestFlowRequest {
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface SuggestFlowResponse {
  model: string
  result: SuggestFlowStatus
  session_id: SessionId
  turn_count: number
  [k: string]: unknown
}
export interface InstallSuggestedFlowRequest {
  flow_name: string
  request_id?: null | RequestId
  session_id: SessionId
  source: string
  [k: string]: unknown
}
export interface InstallSuggestedFlowResponse {
  flow_name: string
  session_id: SessionId
  [k: string]: unknown
}
export interface MoveSessionRequest {
  project_root: string
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface MoveSessionResponse {
  cursor: EventCursor
  revision: Revision
  session: SessionSummary
  [k: string]: unknown
}
export interface SetSessionGoalRequest {
  goal?: string | null
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface SetSessionGoalResponse {
  cursor: EventCursor
  goal?: string | null
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface UpdateSessionTodosRequest {
  mutation: TodoMutation
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface UpdateSessionTodosResponse {
  cursor: EventCursor
  revision: Revision
  session_id: SessionId
  todos: TodoProjection[]
  [k: string]: unknown
}
export interface ListProjectsRequest {
  limit?: number | null
  search?: string | null
  [k: string]: unknown
}
export interface ListProjectsResponse {
  projects: ProjectSummary[]
  total: number
  [k: string]: unknown
}
export interface ProjectSummary {
  active_session_count: number
  id: ProjectId
  last_session_at?: string | null
  name: string
  root: string
  session_count: number
  [k: string]: unknown
}
export interface ListSessionsRequest {
  limit?: number | null
  project_root?: string | null
  search?: string | null
  [k: string]: unknown
}
export interface RenameSessionRequest {
  request_id?: null | RequestId
  session_id: SessionId
  title?: string | null
  [k: string]: unknown
}
export interface RenameSessionResponse {
  cursor: EventCursor
  revision: Revision
  session: SessionSummary
  [k: string]: unknown
}
export interface StartRunRequest {
  args?: {
    [k: string]: unknown
  }
  flow_name?: string | null
  flow_path: string
  images?: InlineImage[]
  reasoning?: string | null
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface StartRunResponse {
  cursor: EventCursor
  revision: Revision
  run_id: FlowRunId
  session_id: SessionId
  [k: string]: unknown
}
export interface RunFlowRequest {
  args?: {
    [k: string]: unknown
  }
  flow_name?: string | null
  flow_path: string
  images?: InlineImage[]
  project_root?: string | null
  reasoning?: string | null
  request_id?: null | RequestId
  [k: string]: unknown
}
export interface RunFlowResponse {
  cursor: EventCursor
  revision: Revision
  run_id: FlowRunId
  session_id: SessionId
  [k: string]: unknown
}
export interface CancelRunRequest {
  request_id?: null | RequestId
  run_id: FlowRunId
  session_id: SessionId
  [k: string]: unknown
}
export interface CancelRunResponse {
  cancelled: boolean
  cursor: EventCursor
  revision: Revision
  run_id: FlowRunId
  session_id: SessionId
  status: RunCancellationStatus
  [k: string]: unknown
}
export interface GetEventsRequest {
  session_id: SessionId
  since_seq?: number | null
  [k: string]: unknown
}
export interface GetEventsResponse {
  events: ServerEventEnvelope[]
  has_more: boolean
  next_cursor: EventCursor
  [k: string]: unknown
}
export interface ServerEventEnvelope {
  cursor: EventCursor
  event: {
    [k: string]: unknown
  }
  schema_version: number
  [k: string]: unknown
}
export interface GetSessionSnapshotRequest {
  session_id: SessionId
  [k: string]: unknown
}
export interface GetSessionUpdatesRequest {
  after_cursor?: EventCursor
  limit?: number | null
  session_id: SessionId
  [k: string]: unknown
}
export interface GetSessionUpdatesResponse {
  daemon_generation: DaemonGeneration
  events: ProjectionEventEnvelope[]
  has_more: boolean
  next_cursor: EventCursor
  resync_required?: null | ResyncRequired
  [k: string]: unknown
}
export interface ProjectionEventEnvelope {
  cursor: EventCursor
  daemon_generation: DaemonGeneration
  event: ServerEvent
  schema_version: number
  session_id: SessionId
  ts: string
  [k: string]: unknown
}
export interface ProjectionDelta {
  base_revision: Revision
  changes: ProjectionChange[]
  revision: Revision
  [k: string]: unknown
}
export interface SessionNotification {
  level: NoticeLevel
  lifecycle: NotificationLifecycle
  location: NotificationLocation
  message: string
  run_id?: null | FlowRunId
  stack: NotificationStack
  [k: string]: unknown
}
export interface ResyncRequired {
  available_from: EventCursor
  reason: string
  requested_after: EventCursor
  snapshot_revision: Revision
  [k: string]: unknown
}
export interface SessionTimelineBudget {
  byte_budget?: number | null
  turn_budget?: number | null
  [k: string]: unknown
}
export interface SessionTimelinePage {
  as_of_cursor: EventCursor
  live?: null | TimelineLiveState
  newer: TimelineRemaining
  older: TimelineRemaining
  projection_revision: Revision
  segments?: TimelineSegment[]
  serialized_bytes: number
  session_id: SessionId
  [k: string]: unknown
}
export interface TimelineLiveState {
  active_turns?: TurnId[]
  interactions: InteractionProjection
  lifecycle: SessionLifecycle
  resources?: ResourceProjection[]
  [k: string]: unknown
}
export interface TimelineRemaining {
  estimated_segments?: number | null
  has_more: boolean
  [k: string]: unknown
}
export interface TimelineTurnSegment {
  id: TimelineSegmentId
  items?: TimelineItem[]
  latest_seq: number
  revision: Revision
  start_seq: number
  state: TimelineTurnState
  turn_id: TurnId
  workflow?: null | WorkflowProjection
  [k: string]: unknown
}
export interface TimelineItem {
  detail?: null | TimelineDetailRef
  id: TimelineItemId
  kind: TimelineItemKind
  preview: TranscriptItem
  run_id?: null | FlowRunId
  seq: number
  turn_id?: null | TurnId
  [k: string]: unknown
}
export interface TimelineDetailRef {
  item_id: TimelineItemId
  [k: string]: unknown
}
export interface TimelineSessionSegment {
  id: TimelineSegmentId
  items?: TimelineItem[]
  latest_seq: number
  revision: Revision
  start_seq: number
  [k: string]: unknown
}
export interface TimelineCursor {
  item_id: TimelineItemId
  seq: number
  [k: string]: unknown
}
export interface GetSessionTimelineItemDetailRequest {
  item_id: TimelineItemId
  session_id: SessionId
  [k: string]: unknown
}
export interface SessionTimelineItemDetail {
  item: TranscriptItem
  item_id: TimelineItemId
  session_id: SessionId
  [k: string]: unknown
}
export interface ResolvePromptRequest {
  answer: unknown
  prompt_id: PromptId
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface ResolvePromptResponse {
  cursor: EventCursor
  prompt_id: PromptId
  resolved: boolean
  revision: Revision
  session_id: SessionId
  status: PromptResolutionStatus
  [k: string]: unknown
}
export interface SubmitFormRequest {
  form_id: string
  request_id?: null | RequestId
  session_id: SessionId
  submission: FormSubmission
  [k: string]: unknown
}
export interface SubmitFormResponse {
  cursor: EventCursor
  form_id: string
  resolved: boolean
  revision: Revision
  session_id: SessionId
  status: FormResolutionStatus
  [k: string]: unknown
}
export interface CompactSessionRequest {
  request_id?: null | RequestId
  session_id: SessionId
  [k: string]: unknown
}
export interface CompactSessionResponse {
  cursor: EventCursor
  operation_id?: null | CompactionOperationId
  revision: Revision
  session_id: SessionId
  status: CompactionRequestStatus
  [k: string]: unknown
}
export interface ResolveCompactReviewRequest {
  decision: CompactReviewDecision
  request_id?: null | RequestId
  review_id: string
  session_id: SessionId
  [k: string]: unknown
}
export interface ResolveCompactReviewResponse {
  cursor: EventCursor
  resolved: boolean
  review_id: string
  revision: Revision
  session_id: SessionId
  status: CompactReviewResolutionStatus
  [k: string]: unknown
}
export interface ListPermissionRequestsRequest {
  session_id: SessionId
  [k: string]: unknown
}
export interface ListPermissionRequestsResponse {
  cursor: EventCursor
  groups: ApprovalGroupProjection[]
  requests: ApprovalRequestProjection[]
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface CreatePermissionGroupRequest {
  expected_request_revisions: {
    [k: string]: number
  }
  label: string
  request_id?: null | RequestId
  request_ids: string[]
  session_id: SessionId
  [k: string]: unknown
}
export interface CreatePermissionGroupResponse {
  cursor: EventCursor
  group_id: string
  label: string
  request_ids: string[]
  revision: number
  session_id: SessionId
  session_revision: Revision
  [k: string]: unknown
}
export interface ResolvePermissionRequestsRequest {
  action: PermissionRpcAction
  reason?: string | null
  request_id?: null | RequestId
  scope?: null | PermissionRpcScope
  selector: PermissionRpcSelector
  session_id: SessionId
  [k: string]: unknown
}
export interface ResolvePermissionRequestsResponse {
  cursor: EventCursor
  resolutions: PermissionResolutionView[]
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface PermissionResolutionView {
  outcome: string
  request_id: string
  [k: string]: unknown
}
export interface ListResourcesRequest {
  session_id: SessionId
  [k: string]: unknown
}
export interface ListResourcesResponse {
  cursor: EventCursor
  resources: ResourceProjection[]
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface InspectResourceRequest {
  resource_id: ResourceId
  session_id: SessionId
  [k: string]: unknown
}
export interface InspectResourceResponse {
  cursor: EventCursor
  resource: ResourceProjection
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface TerminateResourceRequest {
  request_id?: null | RequestId
  resource_id: ResourceId
  session_id: SessionId
  [k: string]: unknown
}
export interface TerminateResourceResponse {
  cursor: EventCursor
  resource_id: ResourceId
  revision: Revision
  session_id: SessionId
  status: ResourceTerminationStatus
  [k: string]: unknown
}
export interface ResizeTerminalResourceRequest {
  cols: number
  request_id?: null | RequestId
  resource_id: ResourceId
  rows: number
  session_id: SessionId
  [k: string]: unknown
}
export interface ResizeTerminalResourceResponse {
  cols: number
  cursor: EventCursor
  resource_id: ResourceId
  revision: Revision
  rows: number
  session_id: SessionId
  status: TerminalResizeStatus
  [k: string]: unknown
}
export interface RetainResourceRequest {
  request_id?: null | RequestId
  resource_id: ResourceId
  session_id: SessionId
  [k: string]: unknown
}
export interface RetainResourceResponse {
  cursor: EventCursor
  resource: ResourceProjection
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface ReleaseResourceRequest {
  request_id?: null | RequestId
  resource_id: ResourceId
  session_id: SessionId
  [k: string]: unknown
}
export interface ReleaseResourceResponse {
  cursor: EventCursor
  resource: ResourceProjection
  revision: Revision
  session_id: SessionId
  [k: string]: unknown
}
export interface JsonRpcRequest {
  id?: unknown
  jsonrpc: string
  method: string
  params?: {
    [k: string]: unknown
  } | null
  [k: string]: unknown
}
export interface JsonRpcResponse {
  error?: null | JsonRpcError
  id?: unknown
  jsonrpc: string
  result?: unknown
  [k: string]: unknown
}
export interface JsonRpcError {
  code: number
  data?: unknown
  message: string
  [k: string]: unknown
}
