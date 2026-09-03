import type { JsonRpcResponse } from './generated/types.generated'
import type {
  RpcMethodName,
  RpcMethodParams,
} from './generated/methods.generated'

export interface RpcRequestEnvelope<M extends RpcMethodName = RpcMethodName> {
  jsonrpc: '2.0'
  id: number
  method: M
  params: RpcMethodParams<M>
}

export interface TransportRequestOptions {
  signal?: AbortSignal
}

export interface RpcTransport {
  send<M extends RpcMethodName>(
    request: RpcRequestEnvelope<M>,
    options?: TransportRequestOptions,
  ): Promise<JsonRpcResponse>
}
