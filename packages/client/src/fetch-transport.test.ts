import { describe, expect, test } from 'bun:test'

import { AtmanHttpError } from './errors'
import { FetchTransport } from './fetch-transport'

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
})
