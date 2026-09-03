import { describe, expect, test } from 'bun:test'

import { AtmanClient } from './client'
import { AtmanTransportError, SessionReconcileError } from './errors'
import {
  EVENT_SCHEMA_VERSION,
  PROTOCOL_VERSION,
  SNAPSHOT_SCHEMA_VERSION,
} from './generated/methods.generated'
import type {
  CapabilitiesResponse,
  GetSessionUpdatesResponse,
  JsonRpcResponse,
  ProjectionChange,
  ProjectionEventEnvelope,
  ServerEvent,
  SessionSnapshot,
} from './generated/types.generated'
import type {
  RpcRequestEnvelope,
  RpcTransport,
  TransportRequestOptions,
} from './transport'

const sessionId = '00000000-0000-0000-0000-000000000001'

class SessionTransport implements RpcTransport {
  readonly requests: RpcRequestEnvelope[] = []
  readonly streamCursors: number[] = []

  constructor(
    readonly respond: (
      request: RpcRequestEnvelope,
      options?: TransportRequestOptions,
    ) => JsonRpcResponse | Promise<JsonRpcResponse>,
    readonly stream?: (
      afterCursor: number,
      options?: TransportRequestOptions,
    ) => AsyncIterable<ProjectionEventEnvelope>,
  ) {}

  async send<M extends RpcRequestEnvelope['method']>(
    request: RpcRequestEnvelope<M>,
    options?: TransportRequestOptions,
  ): Promise<JsonRpcResponse> {
    this.requests.push(request as RpcRequestEnvelope)
    return this.respond(request as RpcRequestEnvelope, options)
  }

  sessionEvents(
    _sessionId: string,
    afterCursor: number,
    options?: TransportRequestOptions,
  ): AsyncIterable<ProjectionEventEnvelope> {
    this.streamCursors.push(afterCursor)
    return this.stream?.(afterCursor, options) ?? emptyEvents()
  }
}

function capabilities(
  generation: string,
  additionalMethods: CapabilitiesResponse['methods'] = [],
): CapabilitiesResponse {
  return {
    protocol_version: PROTOCOL_VERSION,
    daemon_version: 'test',
    daemon_generation: generation,
    snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
    event_schema_version: EVENT_SCHEMA_VERSION,
    methods: [
      { name: 'daemon.capabilities', kind: 'query', revision: 1 },
      { name: 'session.get_snapshot', kind: 'query', revision: 1 },
      { name: 'session.get_updates', kind: 'query', revision: 1 },
      { name: 'session.send_message', kind: 'command', revision: 1 },
      ...additionalMethods,
    ],
    limits: { max_event_page_size: 100, subscriber_buffer: 256 },
  }
}

function snapshot(
  generation: string,
  cursor = 0,
  revision = 0,
  title = 'initial',
  id = sessionId,
): SessionSnapshot {
  return {
    schema_version: SNAPSHOT_SCHEMA_VERSION,
    daemon_generation: generation,
    cursor,
    projection: {
      revision,
      lifecycle: 'idle',
      metadata: { id, title },
      transcript: [],
      runs: [],
      resources: [],
    },
  }
}

function event(
  generation: string,
  cursor: number,
  payload: ServerEvent,
): ProjectionEventEnvelope {
  return {
    schema_version: EVENT_SCHEMA_VERSION,
    daemon_generation: generation,
    session_id: sessionId,
    cursor,
    ts: '2026-09-03T00:00:00Z',
    event: payload,
  }
}

function delta(revision: number, changes: ProjectionChange[]): ServerEvent {
  return {
    type: 'projection_delta',
    delta: { base_revision: revision - 1, revision, changes },
  }
}

function page(
  generation: string,
  afterCursor: number,
  events: ProjectionEventEnvelope[] = [],
  overrides: Partial<GetSessionUpdatesResponse> = {},
): GetSessionUpdatesResponse {
  return {
    daemon_generation: generation,
    events,
    next_cursor: events.at(-1)?.cursor ?? afterCursor,
    has_more: false,
    ...overrides,
  }
}

function result(request: RpcRequestEnvelope, value: unknown): JsonRpcResponse {
  return { jsonrpc: '2.0', id: request.id, result: value }
}

async function* emptyEvents(): AsyncIterable<ProjectionEventEnvelope> {}

describe('SessionClient', () => {
  test('attaches to one session and rejects a mismatched snapshot identity', async () => {
    const transport = new SessionTransport((request) =>
      result(
        request,
        request.method === 'daemon.capabilities'
          ? capabilities('generation-1')
          : snapshot('generation-1', 0, 0, 'wrong', 'other-session'),
      ),
    )
    const client = await AtmanClient.connect(transport, {
      id: 'client-id',
      name: 'browser-test',
      version: '1.0.0',
    })

    await expect(client.attachSession(sessionId)).rejects.toBeInstanceOf(
      SessionReconcileError,
    )
  })

  test('recovers a retention gap from a fresh snapshot', async () => {
    let snapshotCalls = 0
    const transport = new SessionTransport((request) => {
      switch (request.method) {
        case 'daemon.capabilities':
          return result(request, capabilities('generation-1'))
        case 'session.get_snapshot':
          snapshotCalls += 1
          return result(
            request,
            snapshotCalls === 1
              ? snapshot('generation-1')
              : snapshot('generation-1', 10, 4, 'resynced'),
          )
        case 'session.get_updates':
          return result(
            request,
            page('generation-1', 0, [], {
              next_cursor: 10,
              resync_required: {
                available_from: 8,
                requested_after: 0,
                snapshot_revision: 4,
                reason: 'retention gap',
              },
            }),
          )
        default:
          throw new Error(`unexpected method ${request.method}`)
      }
    })
    const client = await AtmanClient.connect(transport, {
      name: 'browser-test',
      version: '1.0.0',
    })
    const session = await client.attachSession(sessionId)

    expect(await session.refresh()).toEqual({ kind: 'resynced' })
    expect(session.current.cursor).toBe(10)
    expect(session.current.projection.metadata.title).toBe('resynced')
  })

  test('re-handshakes and replaces state after the daemon generation changes', async () => {
    let capabilityCalls = 0
    let snapshotCalls = 0
    const transport = new SessionTransport((request) => {
      switch (request.method) {
        case 'daemon.capabilities':
          capabilityCalls += 1
          return result(
            request,
            capabilities(capabilityCalls === 1 ? 'generation-1' : 'generation-2'),
          )
        case 'session.get_snapshot':
          snapshotCalls += 1
          return result(
            request,
            snapshotCalls === 1
              ? snapshot('generation-1')
              : snapshot('generation-2', 5, 2, 'reconnected'),
          )
        case 'session.get_updates':
          return result(request, page('generation-2', 5))
        default:
          throw new Error(`unexpected method ${request.method}`)
      }
    })
    const client = await AtmanClient.connect(transport, {
      name: 'browser-test',
      version: '1.0.0',
    })
    const session = await client.attachSession(sessionId)

    expect(await session.refresh()).toEqual({ kind: 'reconnected' })
    expect(client.capabilities.daemon_generation).toBe('generation-2')
    expect(session.current.daemon_generation).toBe('generation-2')
    expect(session.current.cursor).toBe(5)
  })

  test('replays missed pages between stream reconnects and converges', async () => {
    let streamCalls = 0
    const first = event(
      'generation-1',
      1,
      delta(1, [{ type: 'lifecycle_set', lifecycle: 'active' }]),
    )
    const missed = event(
      'generation-1',
      2,
      delta(2, [
        {
          type: 'metadata_set',
          metadata: { id: sessionId, title: 'caught up' },
        },
      ]),
    )
    const final = event('generation-1', 3, { type: 'heartbeat' })
    const transport = new SessionTransport(
      (request) => {
        switch (request.method) {
          case 'daemon.capabilities':
            return result(request, capabilities('generation-1'))
          case 'session.get_snapshot':
            return result(request, snapshot('generation-1'))
          case 'session.get_updates': {
            const afterCursor = Number(request.params.after_cursor ?? 0)
            const events = streamCalls === 1 && afterCursor === 1 ? [missed] : []
            return result(request, page('generation-1', afterCursor, events))
          }
          default:
            throw new Error(`unexpected method ${request.method}`)
        }
      },
      () => {
        streamCalls += 1
        return streamCalls === 1 ? oneEventThenDisconnect(first) : oneEvent(final)
      },
    )
    const client = await AtmanClient.connect(transport, {
      name: 'browser-test',
      version: '1.0.0',
    })
    const session = await client.attachSession(sessionId)
    const controller = new AbortController()
    session.subscribe((current) => {
      if (current.cursor === 3) {
        controller.abort()
      }
    })

    await expect(
      session.synchronize({
        signal: controller.signal,
        minReconnectDelayMs: 0,
        maxReconnectDelayMs: 0,
      }),
    ).rejects.toHaveProperty('name', 'AbortError')
    expect(transport.streamCursors).toEqual([0, 2])
    expect(session.current.cursor).toBe(3)
    expect(session.current.projection.metadata.title).toBe('caught up')
  })

  test('retries a command with one business request ID and reconciles its cursor', async () => {
    let commandAttempts = 0
    const committedEvents = [
      event('generation-1', 1, { type: 'heartbeat' }),
      event('generation-1', 2, { type: 'heartbeat' }),
    ]
    const transport = new SessionTransport((request) => {
      switch (request.method) {
        case 'daemon.capabilities':
          return result(request, capabilities('generation-1'))
        case 'session.get_snapshot':
          return result(request, snapshot('generation-1'))
        case 'session.send_message':
          commandAttempts += 1
          if (commandAttempts === 1) {
            throw new AtmanTransportError('connection reset after commit', true)
          }
          return result(request, {
            session_id: sessionId,
            run_id: 'run-1',
            revision: 1,
            cursor: 2,
          })
        case 'session.get_updates':
          return result(request, page('generation-1', 0, committedEvents))
        default:
          throw new Error(`unexpected method ${request.method}`)
      }
    })
    const client = await AtmanClient.connect(transport, {
      name: 'browser-test',
      version: '1.0.0',
    })
    const session = await client.attachSession(sessionId)

    const response = await session.sendMessage('hello')

    const attempts = transport.requests.filter(
      (request) => request.method === 'session.send_message',
    )
    expect(attempts).toHaveLength(2)
    expect(attempts[0]?.id).not.toBe(attempts[1]?.id)
    expect(attempts[0]?.params.request_id).toBe(attempts[1]?.params.request_id)
    expect(response.run_id).toBe('run-1')
    expect(session.current.cursor).toBe(2)
  })

  test('keeps interaction responses reconciled with the session', async () => {
    const transport = new SessionTransport((request) => {
      switch (request.method) {
        case 'daemon.capabilities':
          return result(
            request,
            capabilities('generation-1', [
              { name: 'form.submit', kind: 'command', revision: 1 },
            ]),
          )
        case 'session.get_snapshot':
          return result(request, snapshot('generation-1'))
        case 'form.submit':
          return result(request, {
            session_id: sessionId,
            form_id: 'form-1',
            resolved: true,
            status: 'resolved',
            revision: 1,
            cursor: 1,
          })
        case 'session.get_updates': {
          const afterCursor = Number(request.params.after_cursor ?? 0)
          return result(
            request,
            page('generation-1', afterCursor, [
              event('generation-1', afterCursor + 1, { type: 'heartbeat' }),
            ]),
          )
        }
        default:
          throw new Error(`unexpected method ${request.method}`)
      }
    })
    const client = await AtmanClient.connect(transport, {
      name: 'browser-test',
      version: '1.0.0',
    })
    const session = await client.attachSession(sessionId)

    await session.submitForm('form-1', { status: 'rejected' })
    expect(session.current.cursor).toBe(1)
  })
})

async function* oneEvent(
  value: ProjectionEventEnvelope,
): AsyncIterable<ProjectionEventEnvelope> {
  yield value
}

async function* oneEventThenDisconnect(
  value: ProjectionEventEnvelope,
): AsyncIterable<ProjectionEventEnvelope> {
  yield value
  throw new AtmanTransportError('connection reset', true)
}
