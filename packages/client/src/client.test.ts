import { describe, expect, test } from 'bun:test'

import { AtmanClient } from './client'
import {
  AtmanCompatibilityError,
  AtmanProtocolError,
  AtmanRpcError,
  UnsupportedMethodError,
} from './errors'
import {
  EVENT_SCHEMA_VERSION,
  PROTOCOL_VERSION,
  SNAPSHOT_SCHEMA_VERSION,
} from './generated/methods.generated'
import type { CapabilitiesResponse } from './generated/types.generated'
import { MockTransport, mockRpcRoutes, rpcResult } from './testing'

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
    const transport = new MockTransport(
      mockRpcRoutes({
        'daemon.capabilities': () => capabilities(),
        ping: () => ({ pong: true, version: 'test' }),
      }),
    )
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
    const transport = new MockTransport((request) =>
      rpcResult(
        request,
        capabilities({
          methods: [
            { name: 'daemon.capabilities', kind: 'query', revision: 1 },
          ],
        }),
      ),
    )
    const client = await AtmanClient.connect(transport, {
      id: 'client-id',
      name: 'browser-test',
      version: '1.0.0',
    })

    await expect(client.call('ping', {})).rejects.toBeInstanceOf(UnsupportedMethodError)
    expect(transport.requests).toHaveLength(1)
  })

  test('rejects incompatible protocol and correlation identities', async () => {
    const incompatible = new MockTransport((request) =>
      rpcResult(
        request,
        capabilities({ protocol_version: PROTOCOL_VERSION + 1 }),
      ),
    )
    const incompatibleConnection = AtmanClient.connect(incompatible, {
      id: 'client-id',
      name: 'browser-test',
      version: '1.0.0',
    })
    await expect(incompatibleConnection).rejects.toBeInstanceOf(
      AtmanCompatibilityError,
    )
    await expect(incompatibleConnection).rejects.toMatchObject({
      code: 'protocol',
    })

    const mismatched = new MockTransport((request) => ({
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
    const transport = new MockTransport((request) =>
      request.method === 'daemon.capabilities'
        ? rpcResult(request, capabilities())
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

  test('serializes capability refreshes so an older response cannot win', async () => {
    let capabilityCalls = 0
    let releaseFirst = () => {}
    let markFirstStarted = () => {}
    const firstStarted = new Promise<void>((resolve) => {
      markFirstStarted = resolve
    })
    const firstGate = new Promise<void>((resolve) => {
      releaseFirst = resolve
    })
    const transport = new MockTransport(async (request) => {
      capabilityCalls += 1
      if (capabilityCalls === 1) {
        return rpcResult(request, capabilities())
      }
      if (capabilityCalls === 2) {
        markFirstStarted()
        await firstGate
        return rpcResult(
          request,
          capabilities({ daemon_generation: 'generation-2' }),
        )
      }
      return rpcResult(
        request,
        capabilities({ daemon_generation: 'generation-3' }),
      )
    })
    const client = await AtmanClient.connect(transport, {
      name: 'browser-test',
      version: '1.0.0',
    })

    const first = client.refreshCapabilities()
    await firstStarted
    const second = client.refreshCapabilities()
    await Promise.resolve()
    expect(capabilityCalls).toBe(2)
    releaseFirst()

    expect((await first).daemon_generation).toBe('generation-2')
    expect((await second).daemon_generation).toBe('generation-3')
    expect(client.capabilities.daemon_generation).toBe('generation-3')
  })
})
