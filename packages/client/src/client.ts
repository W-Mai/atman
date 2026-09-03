import {
  AtmanProtocolError,
  AtmanRpcError,
  UnsupportedMethodError,
} from './errors'
import {
  EVENT_SCHEMA_VERSION,
  PROTOCOL_VERSION,
  RPC_METHODS,
  SNAPSHOT_SCHEMA_VERSION,
  type RpcMethodName,
  type RpcMethodParams,
  type RpcMethodResult,
} from './generated/methods.generated'
import type {
  CapabilitiesRequest,
  CapabilitiesResponse,
  EventCursor,
  MethodCapability,
  ProjectionEventEnvelope,
  SessionId,
} from './generated/types.generated'
import type {
  RpcRequestEnvelope,
  RpcTransport,
  TransportRequestOptions,
} from './transport'

export interface ClientIdentity {
  id?: string
  name: string
  version: string
}

export class AtmanClient {
  readonly #transport: RpcTransport
  readonly #identity: Required<ClientIdentity>
  #capabilities: CapabilitiesResponse
  #nextRequestId = 2

  private constructor(
    transport: RpcTransport,
    identity: Required<ClientIdentity>,
    capabilities: CapabilitiesResponse,
  ) {
    this.#transport = transport
    this.#identity = identity
    this.#capabilities = capabilities
  }

  static async connect(
    transport: RpcTransport,
    identity: ClientIdentity,
    options: TransportRequestOptions = {},
  ): Promise<AtmanClient> {
    const resolvedIdentity = {
      id: identity.id ?? crypto.randomUUID(),
      name: identity.name,
      version: identity.version,
    }
    const params: CapabilitiesRequest = {
      client_id: resolvedIdentity.id,
      client_name: resolvedIdentity.name,
      client_version: resolvedIdentity.version,
      protocol_version: PROTOCOL_VERSION,
    }
    const capabilities = await invoke(
      transport,
      1,
      'daemon.capabilities',
      params,
      options,
    )
    validateCapabilities(capabilities)
    return new AtmanClient(transport, resolvedIdentity, capabilities)
  }

  get capabilities(): CapabilitiesResponse {
    return {
      ...this.#capabilities,
      limits: { ...this.#capabilities.limits },
      methods: this.#capabilities.methods.map((method) => ({ ...method })),
    }
  }

  supports(method: RpcMethodName): boolean {
    return supports(this.#capabilities, method)
  }

  sessionEvents(
    sessionId: SessionId,
    afterCursor: EventCursor,
    options: TransportRequestOptions = {},
  ): AsyncIterable<ProjectionEventEnvelope> | undefined {
    return this.#transport.sessionEvents?.(sessionId, afterCursor, options)
  }

  async refreshCapabilities(
    options: TransportRequestOptions = {},
  ): Promise<CapabilitiesResponse> {
    const capabilities = await this.#invoke(
      'daemon.capabilities',
      {
        client_id: this.#identity.id,
        client_name: this.#identity.name,
        client_version: this.#identity.version,
        protocol_version: PROTOCOL_VERSION,
      },
      options,
      false,
    )
    validateCapabilities(capabilities)
    this.#capabilities = capabilities
    return this.capabilities
  }

  async call<M extends RpcMethodName>(
    method: M,
    params: RpcMethodParams<M>,
    options: TransportRequestOptions = {},
  ): Promise<RpcMethodResult<M>> {
    return this.#invoke(method, params, options, true)
  }

  async #invoke<M extends RpcMethodName>(
    method: M,
    params: RpcMethodParams<M>,
    options: TransportRequestOptions,
    requireCapability: boolean,
  ): Promise<RpcMethodResult<M>> {
    if (requireCapability && !supports(this.#capabilities, method)) {
      throw new UnsupportedMethodError(method, RPC_METHODS[method].revision)
    }
    const requestId = this.#nextRequestId++
    return invoke(this.#transport, requestId, method, params, options)
  }
}

async function invoke<M extends RpcMethodName>(
  transport: RpcTransport,
  id: number,
  method: M,
  params: RpcMethodParams<M>,
  options: TransportRequestOptions,
): Promise<RpcMethodResult<M>> {
  const request: RpcRequestEnvelope<M> = {
    jsonrpc: '2.0',
    id,
    method,
    params,
  }
  const response = await transport.send(request, options)
  if (response.id !== id) {
    throw new AtmanProtocolError(
      `atman daemon response correlation mismatch: expected ${id}, received ${String(response.id)}`,
    )
  }
  if (response.error) {
    throw new AtmanRpcError(response.error)
  }
  if (!Object.hasOwn(response, 'result')) {
    throw new AtmanProtocolError('atman daemon response has no result')
  }
  return response.result as RpcMethodResult<M>
}

function supports(capabilities: CapabilitiesResponse, method: RpcMethodName): boolean {
  const expected = RPC_METHODS[method]
  return capabilities.methods.some(
    (candidate) =>
      candidate.name === method &&
      candidate.kind === expected.kind &&
      candidate.revision >= expected.revision,
  )
}

function validateCapabilities(capabilities: CapabilitiesResponse): void {
  if (capabilities.protocol_version !== PROTOCOL_VERSION) {
    throw new AtmanProtocolError(
      `atman daemon protocol version ${capabilities.protocol_version} is incompatible with client version ${PROTOCOL_VERSION}`,
    )
  }
  if (
    supportsMethod(capabilities.methods, 'session.get_snapshot') &&
    capabilities.snapshot_schema_version !== SNAPSHOT_SCHEMA_VERSION
  ) {
    throw new AtmanProtocolError(
      `atman daemon snapshot schema version ${String(capabilities.snapshot_schema_version)} is incompatible with client version ${SNAPSHOT_SCHEMA_VERSION}`,
    )
  }
  if (
    supportsMethod(capabilities.methods, 'session.get_updates') &&
    capabilities.event_schema_version !== EVENT_SCHEMA_VERSION
  ) {
    throw new AtmanProtocolError(
      `atman daemon event schema version ${capabilities.event_schema_version} is incompatible with client version ${EVENT_SCHEMA_VERSION}`,
    )
  }
}

function supportsMethod(methods: MethodCapability[], method: RpcMethodName): boolean {
  const expected = RPC_METHODS[method]
  return methods.some(
    (candidate) =>
      candidate.name === method &&
      candidate.kind === expected.kind &&
      candidate.revision >= expected.revision,
  )
}
