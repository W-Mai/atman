import { describe, expect, test } from 'bun:test'

import { AtmanClient } from './client'
import {
  AtmanProtocolError,
  AtmanRpcError,
  UnsupportedMethodError,
} from './errors'
import {
  EVENT_SCHEMA_VERSION,
  PROTOCOL_VERSION,
  SNAPSHOT_SCHEMA_VERSION,
} from './generated/methods.generated'
import type {
  CapabilitiesResponse,
  JsonRpcResponse,
} from './generated/types.generated'
import type {
  RpcRequestEnvelope,
  RpcTransport,
  TransportRequestOptions,
} from './transport'

class FakeTransport implements RpcTransport {
  readonly requests: RpcRequestEnvelope[] = []

  constructor(
    readonly respond: (
      request: RpcRequestEnvelope,
      options?: TransportRequestOptions,
    ) => JsonRpcResponse,
  ) {}

  async send<M extends RpcRequestEnvelope['method']>(
    request: RpcRequestEnvelope<M>,
    options?: TransportRequestOptions,
  ): Promise<JsonRpcResponse> {
    this.requests.push(request as RpcRequestEnvelope)
    return this.respond(request as RpcRequestEnvelope, options)
  }
}

function capabilities(
  overrides: Partial<CapabilitiesResponse> = {},
): CapabilitiesResponse {
  return {
    protocol_version: PROTOCOL_VERSION,
    daemon_version: 'test',
    daemon_generation: 'generation',
    snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
    event_schema_version: EVENT_SCHEMA_VERSION,
    methods: [
      { name: 'daemon.capabilities', kind: 'query', revision: 1 },
      { name: 'ping', kind: 'query', revision: 1 },
    ],
    limits: { max_event_page_size: 100, subscriber_buffer: 256 },
    ...overrides,
  }
}

describe('AtmanClient', () => {
  test('handshakes before typed calls and correlates responses', async () => {
    const transport = new FakeTransport((request) => ({
      jsonrpc: '2.0',
      id: request.id,
      result:
        request.method === 'daemon.capabilities'
          ? capabilities()
          : { pong: true, version: 'test' },
    }))
    const client = await AtmanClient.connect(transport, {
      id: 'client-id',
      name: 'browser-test',
      version: '1.0.0',
    })

    expect(await client.call('ping', {})).toEqual({ pong: true, version: 'test' })
    expect(transport.requests.map((request) => request.method)).toEqual([
      'daemon.capabilities',
      'ping',
    ])
    expect(transport.requests[0]?.params).toEqual({
      client_id: 'client-id',
      client_name: 'browser-test',
      client_version: '1.0.0',
      protocol_version: PROTOCOL_VERSION,
    })
  })

  test('rejects unsupported methods before sending', async () => {
    const transport = new FakeTransport((request) => ({
      jsonrpc: '2.0',
      id: request.id,
      result: capabilities({ methods: [{ name: 'daemon.capabilities', kind: 'query', revision: 1 }] }),
    }))
    const client = await AtmanClient.connect(transport, {
      id: 'client-id',
      name: 'browser-test',
      version: '1.0.0',
    })

    await expect(client.call('ping', {})).rejects.toBeInstanceOf(UnsupportedMethodError)
    expect(transport.requests).toHaveLength(1)
  })

  test('rejects incompatible protocol and correlation identities', async () => {
    const incompatible = new FakeTransport((request) => ({
      jsonrpc: '2.0',
      id: request.id,
      result: capabilities({ protocol_version: PROTOCOL_VERSION + 1 }),
    }))
    await expect(
      AtmanClient.connect(incompatible, {
        id: 'client-id',
        name: 'browser-test',
        version: '1.0.0',
      }),
    ).rejects.toBeInstanceOf(AtmanProtocolError)

    const mismatched = new FakeTransport((request) => ({
      jsonrpc: '2.0',
      id: request.id + 1,
      result: capabilities(),
    }))
    await expect(
      AtmanClient.connect(mismatched, {
        id: 'client-id',
        name: 'browser-test',
        version: '1.0.0',
      }),
    ).rejects.toBeInstanceOf(AtmanProtocolError)
  })

  test('surfaces typed JSON-RPC errors', async () => {
    const transport = new FakeTransport((request) =>
      request.method === 'daemon.capabilities'
        ? { jsonrpc: '2.0', id: request.id, result: capabilities() }
        : {
            jsonrpc: '2.0',
            id: request.id,
            error: { code: -32000, message: 'unavailable', data: false },
          },
    )
    const client = await AtmanClient.connect(transport, {
      id: 'client-id',
      name: 'browser-test',
      version: '1.0.0',
    })

    await expect(client.call('ping', {})).rejects.toBeInstanceOf(AtmanRpcError)
  })
})
