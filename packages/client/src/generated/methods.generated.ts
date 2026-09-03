// Generated from atman-proto. Do not edit.

import type {
  CancelRunRequest,
  CancelRunResponse,
  CapabilitiesRequest,
  CapabilitiesResponse,
  CloseSessionRequest,
  CloseSessionResponse,
  CreatePermissionGroupRequest,
  CreatePermissionGroupResponse,
  CreateSessionRequest,
  DeleteSessionRequest,
  DeleteSessionResponse,
  EmptyParams,
  GetEventsRequest,
  GetEventsResponse,
  GetSessionSnapshotRequest,
  GetSessionUpdatesRequest,
  GetSessionUpdatesResponse,
  InspectResourceRequest,
  InspectResourceResponse,
  InterjectSessionRequest,
  InterjectSessionResponse,
  ListPermissionRequestsRequest,
  ListPermissionRequestsResponse,
  ListProjectsRequest,
  ListProjectsResponse,
  ListResourcesRequest,
  ListResourcesResponse,
  ListSessionsRequest,
  ListSessionsResult,
  PingResponse,
  ReleaseResourceRequest,
  ReleaseResourceResponse,
  RenameSessionRequest,
  RenameSessionResponse,
  ResolveCompactReviewRequest,
  ResolveCompactReviewResponse,
  ResolvePermissionRequestsRequest,
  ResolvePermissionRequestsResponse,
  ResolvePromptRequest,
  ResolvePromptResponse,
  RetainResourceRequest,
  RetainResourceResponse,
  RunFlowRequest,
  RunFlowResponse,
  SendMessageRequest,
  SendMessageResponse,
  SessionSnapshot,
  StartRunRequest,
  StartRunResponse,
  SubmitFormRequest,
  SubmitFormResponse,
  TerminateResourceRequest,
  TerminateResourceResponse,
  UpdateSessionTrustRequest,
  UpdateSessionTrustResponse,
} from './types.generated'

export const PROTOCOL_VERSION = 1 as const
export const SNAPSHOT_SCHEMA_VERSION = 1 as const
export const EVENT_SCHEMA_VERSION = 1 as const

export interface RpcMethodMap {
  'daemon.capabilities': {
    kind: 'query'
    revision: 1
    params: CapabilitiesRequest
    result: CapabilitiesResponse
  }
  'ping': {
    kind: 'query'
    revision: 1
    params: EmptyParams
    result: PingResponse
  }
  'session.create': {
    kind: 'command'
    revision: 1
    params: CreateSessionRequest
    result: SessionSnapshot
  }
  'session.close': {
    kind: 'command'
    revision: 1
    params: CloseSessionRequest
    result: CloseSessionResponse
  }
  'session.delete': {
    kind: 'command'
    revision: 1
    params: DeleteSessionRequest
    result: DeleteSessionResponse
  }
  'session.send_message': {
    kind: 'command'
    revision: 1
    params: SendMessageRequest
    result: SendMessageResponse
  }
  'session.interject': {
    kind: 'command'
    revision: 1
    params: InterjectSessionRequest
    result: InterjectSessionResponse
  }
  'session.update_trust': {
    kind: 'command'
    revision: 1
    params: UpdateSessionTrustRequest
    result: UpdateSessionTrustResponse
  }
  'project.list': {
    kind: 'query'
    revision: 1
    params: ListProjectsRequest
    result: ListProjectsResponse
  }
  'list_sessions': {
    kind: 'query'
    revision: 1
    params: ListSessionsRequest
    result: ListSessionsResult
  }
  'rename_session': {
    kind: 'command'
    revision: 2
    params: RenameSessionRequest
    result: RenameSessionResponse
  }
  'run.start': {
    kind: 'command'
    revision: 1
    params: StartRunRequest
    result: StartRunResponse
  }
  'run_flow': {
    kind: 'command'
    revision: 1
    params: RunFlowRequest
    result: RunFlowResponse
  }
  'cancel_run': {
    kind: 'command'
    revision: 2
    params: CancelRunRequest
    result: CancelRunResponse
  }
  'get_events': {
    kind: 'query'
    revision: 1
    params: GetEventsRequest
    result: GetEventsResponse
  }
  'session.get_snapshot': {
    kind: 'query'
    revision: 1
    params: GetSessionSnapshotRequest
    result: SessionSnapshot
  }
  'session.get_updates': {
    kind: 'query'
    revision: 1
    params: GetSessionUpdatesRequest
    result: GetSessionUpdatesResponse
  }
  'resolve_prompt': {
    kind: 'command'
    revision: 2
    params: ResolvePromptRequest
    result: ResolvePromptResponse
  }
  'form.submit': {
    kind: 'command'
    revision: 1
    params: SubmitFormRequest
    result: SubmitFormResponse
  }
  'compact_review.resolve': {
    kind: 'command'
    revision: 1
    params: ResolveCompactReviewRequest
    result: ResolveCompactReviewResponse
  }
  'list_permission_requests': {
    kind: 'query'
    revision: 2
    params: ListPermissionRequestsRequest
    result: ListPermissionRequestsResponse
  }
  'create_permission_group': {
    kind: 'command'
    revision: 2
    params: CreatePermissionGroupRequest
    result: CreatePermissionGroupResponse
  }
  'resolve_permission_requests': {
    kind: 'command'
    revision: 2
    params: ResolvePermissionRequestsRequest
    result: ResolvePermissionRequestsResponse
  }
  'resource.list': {
    kind: 'query'
    revision: 1
    params: ListResourcesRequest
    result: ListResourcesResponse
  }
  'resource.inspect': {
    kind: 'query'
    revision: 1
    params: InspectResourceRequest
    result: InspectResourceResponse
  }
  'resource.terminate': {
    kind: 'command'
    revision: 1
    params: TerminateResourceRequest
    result: TerminateResourceResponse
  }
  'resource.retain': {
    kind: 'command'
    revision: 1
    params: RetainResourceRequest
    result: RetainResourceResponse
  }
  'resource.release': {
    kind: 'command'
    revision: 1
    params: ReleaseResourceRequest
    result: ReleaseResourceResponse
  }
}

export type RpcMethodName = keyof RpcMethodMap
export type RpcMethodParams<M extends RpcMethodName> = RpcMethodMap[M]['params']
export type RpcMethodResult<M extends RpcMethodName> = RpcMethodMap[M]['result']

export const RPC_METHODS = {
  'daemon.capabilities': { kind: 'query', revision: 1 },
  'ping': { kind: 'query', revision: 1 },
  'session.create': { kind: 'command', revision: 1 },
  'session.close': { kind: 'command', revision: 1 },
  'session.delete': { kind: 'command', revision: 1 },
  'session.send_message': { kind: 'command', revision: 1 },
  'session.interject': { kind: 'command', revision: 1 },
  'session.update_trust': { kind: 'command', revision: 1 },
  'project.list': { kind: 'query', revision: 1 },
  'list_sessions': { kind: 'query', revision: 1 },
  'rename_session': { kind: 'command', revision: 2 },
  'run.start': { kind: 'command', revision: 1 },
  'run_flow': { kind: 'command', revision: 1 },
  'cancel_run': { kind: 'command', revision: 2 },
  'get_events': { kind: 'query', revision: 1 },
  'session.get_snapshot': { kind: 'query', revision: 1 },
  'session.get_updates': { kind: 'query', revision: 1 },
  'resolve_prompt': { kind: 'command', revision: 2 },
  'form.submit': { kind: 'command', revision: 1 },
  'compact_review.resolve': { kind: 'command', revision: 1 },
  'list_permission_requests': { kind: 'query', revision: 2 },
  'create_permission_group': { kind: 'command', revision: 2 },
  'resolve_permission_requests': { kind: 'command', revision: 2 },
  'resource.list': { kind: 'query', revision: 1 },
  'resource.inspect': { kind: 'query', revision: 1 },
  'resource.terminate': { kind: 'command', revision: 1 },
  'resource.retain': { kind: 'command', revision: 1 },
  'resource.release': { kind: 'command', revision: 1 },
} as const satisfies Record<RpcMethodName, { kind: 'command' | 'query'; revision: number }>
