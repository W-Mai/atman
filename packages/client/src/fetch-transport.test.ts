import { describe, expect, test } from 'bun:test'

import { AtmanHttpError, AtmanTransportError } from './errors'
import { FetchTransport } from './fetch-transport'
import { EVENT_SCHEMA_VERSION } from './generated/methods.generated'
import type { ProjectionEventEnvelope } from './generated/types.generated'

describe('FetchTransport', () => {
  test('uses an authorization header without putting credentials in the URL', async () => {
    const controller = new AbortController()
    let capturedUrl = ''
    let capturedInit: RequestInit | undefined
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777/api/?discarded=yes#fragment',
      token: async () => 'secret-token',
      fetch: async (input, init) => {
        capturedUrl = String(input)
        capturedInit = init
        return Response.json({ jsonrpc: '2.0', id: 7, result: { pong: true } })
      },
    })

    await transport.send(
      { jsonrpc: '2.0', id: 7, method: 'ping', params: {} },
      { signal: controller.signal },
    )

    expect(capturedUrl).toBe('http://127.0.0.1:7777/api/rpc')
    expect(capturedUrl).not.toContain('secret-token')
    expect(new Headers(capturedInit?.headers).get('authorization')).toBe(
      'Bearer secret-token',
    )
    expect(capturedInit?.signal).toBe(controller.signal)
  })

  test('omits authorization when no token provider is configured', async () => {
    let capturedInit: RequestInit | undefined
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777',
      fetch: async (_input, init) => {
        capturedInit = init
        return Response.json({ jsonrpc: '2.0', id: 1, result: {} })
      },
    })

    await transport.send({
      jsonrpc: '2.0',
      id: 1,
      method: 'daemon.capabilities',
      params: {},
    })

    expect(new Headers(capturedInit?.headers).has('authorization')).toBeFalse()
    expect(capturedInit?.credentials).toBe('same-origin')
  })

  test('classifies retryable HTTP failures', async () => {
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777',
      fetch: async () => new Response('temporarily unavailable', { status: 503 }),
    })

    try {
      await transport.send({ jsonrpc: '2.0', id: 1, method: 'ping', params: {} })
      throw new Error('expected request to fail')
    } catch (error) {
      expect(error).toBeInstanceOf(AtmanHttpError)
      expect((error as AtmanHttpError).retryable).toBeTrue()
    }
  })

  test('preserves AbortSignal cancellation', async () => {
    const controller = new AbortController()
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777',
      fetch: async () => {
        controller.abort()
        throw new DOMException('cancelled', 'AbortError')
      },
    })

    await expect(
      transport.send(
        { jsonrpc: '2.0', id: 1, method: 'ping', params: {} },
        { signal: controller.signal },
      ),
    ).rejects.toHaveProperty('name', 'AbortError')
  })

  test('streams split projection SSE frames without putting the bearer in the URL', async () => {
    const encoder = new TextEncoder()
    const envelope: ProjectionEventEnvelope = {
      schema_version: EVENT_SCHEMA_VERSION,
      daemon_generation: 'generation',
      session_id: 'session-1',
      cursor: 8,
      ts: '2026-09-03T00:00:00Z',
      event: { type: 'heartbeat', note: '读取' },
    }
    const source = `retry: 1000\r\n\r\nevent: session_event\r\nid: 8\r\ndata: ${JSON.stringify(envelope)}\r\n\r\n`
    let capturedUrl = ''
    let capturedHeaders = new Headers()
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777/api',
      token: 'secret-token',
      fetch: async (input, init) => {
        capturedUrl = String(input)
        capturedHeaders = new Headers(init?.headers)
        const bytes = encoder.encode(source)
        return new Response(
          new ReadableStream({
            start(controller) {
              for (const byte of bytes) {
                controller.enqueue(Uint8Array.of(byte))
              }
              controller.close()
            },
          }),
        )
      },
    })

    const events = []
    for await (const event of transport.sessionEvents('session-1', 7)) {
      events.push(event)
    }

    expect(events).toEqual([envelope])
    expect(capturedUrl).toBe(
      'http://127.0.0.1:7777/api/session-events?session_id=session-1&after_cursor=7',
    )
    expect(capturedUrl).not.toContain('secret-token')
    expect(capturedHeaders.get('authorization')).toBe('Bearer secret-token')
    expect(capturedHeaders.get('last-event-id')).toBe('7')
  })

  test('rejects SSE cursor identity mismatches', async () => {
    const envelope: ProjectionEventEnvelope = {
      schema_version: EVENT_SCHEMA_VERSION,
      daemon_generation: 'generation',
      session_id: 'session-1',
      cursor: 8,
      ts: '2026-09-03T00:00:00Z',
      event: { type: 'heartbeat' },
    }
    const source = `id: 9\ndata: ${JSON.stringify(envelope)}\n\n`
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777',
      fetch: async () => new Response(source),
    })

    await expect(async () => {
      for await (const _event of transport.sessionEvents('session-1', 7)) {
        // Consume the stream.
      }
    }).toThrow('does not match')
  })

  test('classifies event stream disconnects as retryable transport failures', async () => {
    const transport = new FetchTransport({
      baseUrl: 'http://127.0.0.1:7777',
      fetch: async () =>
        new Response(
          new ReadableStream({
            start(controller) {
              controller.error(new Error('connection reset'))
            },
          }),
        ),
    })

    try {
      for await (const _event of transport.sessionEvents('session-1', 7)) {
        // Consume the stream.
      }
      throw new Error('expected event stream to fail')
    } catch (error) {
      expect(error).toBeInstanceOf(AtmanTransportError)
      expect((error as AtmanTransportError).retryable).toBeTrue()
    }
  })
})
