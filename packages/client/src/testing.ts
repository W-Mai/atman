import type {
  RpcMethodName,
  RpcMethodParams,
  RpcMethodResult,
} from './generated/methods.generated'
import type {
  EventCursor,
  JsonRpcResponse,
  ProjectionEventEnvelope,
  SessionId,
} from './generated/types.generated'
import type {
  RpcRequestEnvelope,
  RpcTransport,
  TransportRequestOptions,
} from './transport'

export type MockRpcRequest = {
  [M in RpcMethodName]: RpcRequestEnvelope<M>
}[RpcMethodName]

export interface MockStreamRequest {
  sessionId: SessionId
  afterCursor: EventCursor
  options?: TransportRequestOptions
}

export type MockRpcHandler = (
  request: MockRpcRequest,
  options?: TransportRequestOptions,
) => JsonRpcResponse | Promise<JsonRpcResponse>

export type MockEventHandler = (
  sessionId: SessionId,
  afterCursor: EventCursor,
  options?: TransportRequestOptions,
) => AsyncIterable<ProjectionEventEnvelope>

export type MockRpcRoutes = {
  [M in RpcMethodName]?: (
    params: RpcMethodParams<M>,
    request: RpcRequestEnvelope<M>,
    options?: TransportRequestOptions,
  ) => RpcMethodResult<M> | Promise<RpcMethodResult<M>>
}

export class MockTransport implements RpcTransport {
  readonly requests: MockRpcRequest[] = []
  readonly streams: MockStreamRequest[] = []

  constructor(
    readonly respond: MockRpcHandler,
    readonly events?: MockEventHandler,
  ) {}

  async send<M extends RpcMethodName>(
    request: RpcRequestEnvelope<M>,
    options?: TransportRequestOptions,
  ): Promise<JsonRpcResponse> {
    const captured = request as MockRpcRequest
    this.requests.push(captured)
    return this.respond(captured, options)
  }

  sessionEvents(
    sessionId: SessionId,
    afterCursor: EventCursor,
    options?: TransportRequestOptions,
  ): AsyncIterable<ProjectionEventEnvelope> {
    this.streams.push({
      sessionId,
      afterCursor,
      ...(options ? { options } : {}),
    })
    return this.events?.(sessionId, afterCursor, options) ?? emptyEvents()
  }
}

export function rpcResult<M extends RpcMethodName>(
  request: RpcRequestEnvelope<M>,
  result: RpcMethodResult<M>,
): JsonRpcResponse {
  return { jsonrpc: '2.0', id: request.id, result }
}

export function mockRpcRoutes(routes: MockRpcRoutes): MockRpcHandler {
  return async (request, options) => {
    const route = routes[request.method]
    if (!route) {
      throw new Error(`no mock route for ${request.method}`)
    }
    const invoke = route as unknown as (
      params: unknown,
      request: MockRpcRequest,
      options?: TransportRequestOptions,
    ) => unknown
    const result = await invoke(request.params, request, options)
    return { jsonrpc: '2.0', id: request.id, result }
  }
}

async function* emptyEvents(): AsyncIterable<ProjectionEventEnvelope> {}
