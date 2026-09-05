import {
  AtmanCompatibilityError,
  AtmanProtocolError,
  AtmanRpcError,
  AtmanTransportError,
  UnsupportedMethodError,
} from './errors'
import {
  EVENT_SCHEMA_VERSION,
  PROTOCOL_VERSION,
  RPC_METHODS,
  SNAPSHOT_SCHEMA_VERSION,
  type RpcMethodName,
  type RpcMethodMap,
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
import { SessionClient } from './session-client'

export interface ClientIdentity {
  id?: string
  name: string
  version: string
}

type RpcCommandMethodName = {
  [M in RpcMethodName]: RpcMethodMap[M]['kind'] extends 'command' ? M : never
}[RpcMethodName]

export class AtmanClient {
  readonly #transport: RpcTransport
  readonly #identity: Required<ClientIdentity>
  #capabilities: CapabilitiesResponse
  #nextRequestId = 2
  #capabilityRefreshTail: Promise<void> = Promise.resolve()

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

  async attachSession(
    sessionId: SessionId,
    options: TransportRequestOptions = {},
  ): Promise<SessionClient> {
    return SessionClient.attach(this, sessionId, options)
  }

  async refreshCapabilities(
    options: TransportRequestOptions = {},
  ): Promise<CapabilitiesResponse> {
    return this.#exclusiveCapabilityRefresh(async () => {
      options.signal?.throwIfAborted()
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
    })
  }

  async call<M extends RpcMethodName>(
    method: M,
    params: RpcMethodParams<M>,
    options: TransportRequestOptions = {},
  ): Promise<RpcMethodResult<M>> {
    return this.#invoke(method, params, options, true)
  }

  /** @internal Commands retry once with the caller-provided business request ID. */
  async command<M extends RpcCommandMethodName>(
    method: M,
    params: RpcMethodParams<M>,
    options: TransportRequestOptions = {},
  ): Promise<RpcMethodResult<M>> {
    if (RPC_METHODS[method].kind !== 'command') {
      throw new AtmanProtocolError(`${method} is not a command`)
    }
    try {
      return await this.call(method, params, options)
    } catch (error) {
      if (!(error instanceof AtmanTransportError) || !error.retryable) {
        throw error
      }
      return this.call(method, params, options)
    }
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

  async #exclusiveCapabilityRefresh<T>(operation: () => Promise<T>): Promise<T> {
    const previous = this.#capabilityRefreshTail
    let release = () => {}
    this.#capabilityRefreshTail = new Promise<void>((resolve) => {
      release = resolve
    })
    await previous
    try {
      return await operation()
    } finally {
      release()
    }
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
    throw new AtmanCompatibilityError(
      'protocol',
      `atman client and daemon are incompatible: protocol client=${PROTOCOL_VERSION}, daemon=${capabilities.protocol_version}. Restart the daemon from the same atman installation as this client`,
      { client: PROTOCOL_VERSION, daemon: capabilities.protocol_version },
    )
  }
  if (
    supportsMethod(capabilities.methods, 'session.get_snapshot') &&
    capabilities.snapshot_schema_version !== SNAPSHOT_SCHEMA_VERSION
  ) {
    throw new AtmanCompatibilityError(
      'snapshot_schema',
      `atman client and daemon are incompatible: snapshot schema client=${SNAPSHOT_SCHEMA_VERSION}, daemon=${String(capabilities.snapshot_schema_version)}. Restart the daemon from the same atman installation as this client`,
      {
        client: SNAPSHOT_SCHEMA_VERSION,
        daemon: capabilities.snapshot_schema_version,
      },
    )
  }
  if (
    supportsMethod(capabilities.methods, 'session.get_updates') &&
    capabilities.event_schema_version !== EVENT_SCHEMA_VERSION
  ) {
    throw new AtmanCompatibilityError(
      'event_schema',
      `atman client and daemon are incompatible: event schema client=${EVENT_SCHEMA_VERSION}, daemon=${capabilities.event_schema_version}. Restart the daemon from the same atman installation as this client`,
      { client: EVENT_SCHEMA_VERSION, daemon: capabilities.event_schema_version },
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
