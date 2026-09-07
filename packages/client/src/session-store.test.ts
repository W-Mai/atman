import { describe, expect, test } from 'bun:test'

import { SessionReconcileError } from './errors'
import { EVENT_SCHEMA_VERSION, SNAPSHOT_SCHEMA_VERSION } from './generated/methods.generated'
import type {
  CompactionProjection,
  GetSessionUpdatesResponse,
  InterjectionProjection,
  ProjectionChange,
  ProjectionEventEnvelope,
  ServerEvent,
  SessionSnapshot,
  TranscriptItem,
} from './generated/types.generated'
import { SessionStore } from './session-store'

const sessionId = '00000000-0000-0000-0000-000000000001'
const generation = 'generation-1'

function snapshot(overrides: Partial<SessionSnapshot> = {}): SessionSnapshot {
  return {
    schema_version: SNAPSHOT_SCHEMA_VERSION,
    daemon_generation: generation,
    cursor: 0,
    projection: {
      revision: 0,
      lifecycle: 'idle',
      metadata: { id: sessionId, title: 'initial' },
      transcript: [],
      runs: [],
      resources: [],
    },
    ...overrides,
  }
}

function event(
  cursor: number,
  payload: ServerEvent,
  overrides: Partial<ProjectionEventEnvelope> = {},
): ProjectionEventEnvelope {
  return {
    schema_version: EVENT_SCHEMA_VERSION,
    daemon_generation: generation,
    session_id: sessionId,
    cursor,
    ts: '2026-09-03T00:00:00Z',
    event: payload,
    ...overrides,
  }
}

function delta(revision: number, changes: ProjectionChange[]): ServerEvent {
  return {
    type: 'projection_delta',
    delta: { base_revision: revision - 1, revision, changes },
  }
}

function page(
  events: ProjectionEventEnvelope[],
  overrides: Partial<GetSessionUpdatesResponse> = {},
): GetSessionUpdatesResponse {
  return {
    daemon_generation: generation,
    events,
    next_cursor: events.at(-1)?.cursor ?? 0,
    has_more: false,
    ...overrides,
  }
}

describe('SessionStore', () => {
  test('keeps transcript replacements inside the loaded history window', () => {
    const visible: TranscriptItem = {
      type: 'message',
      seq: 8,
      ts: '2026-09-07T00:00:00Z',
      message: {
        role: 'user',
        origin: 'user',
        turn_id: 'turn-2',
        parts: [{ type: 'text', text: 'visible' }],
      },
    }
    const hidden: TranscriptItem = {
      type: 'message',
      seq: 1,
      ts: '2026-09-06T00:00:00Z',
      message: {
        role: 'user',
        origin: 'user',
        turn_id: 'turn-1',
        parts: [{ type: 'text', text: 'outside window' }],
      },
    }
    const store = new SessionStore(snapshot({
      projection: {
        revision: 0,
        lifecycle: 'idle',
        metadata: { id: sessionId, title: 'bounded' },
        transcript: [visible],
        runs: [],
        resources: [],
      },
    }), { boundedTranscript: true })

    store.applyUpdates(page([event(1, delta(1, [{
      type: 'transcript_replace',
      items: [hidden, visible],
    }]))]))

    expect(store.current.projection.transcript).toEqual([visible])
  })

  test('reconciles active compaction progress by operation identity', () => {
    const store = new SessionStore(snapshot())
    const active: CompactionProjection = {
      id: '00000000-0000-0000-0000-000000000010',
      started_seq: 4,
      context_id: '00000000-0000-0000-0000-000000000011',
      run_id: '00000000-0000-0000-0000-000000000012',
      range_start: 2,
      range_end: 8,
      before_tokens: 10_000,
      compacted_count: 7,
      summary: 'partial',
      started_at: '2026-09-03T00:00:00Z',
    }
    store.applyUpdates(page([event(1, delta(1, [
      { type: 'compactions_replace', compactions: [active] },
    ]))]))
    active.summary = 'mutated outside the store'
    expect(store.current.projection.compactions).toHaveLength(1)
    expect(store.current.projection.compactions?.[0]?.summary).toBe('partial')

    store.applyUpdates(page([event(2, delta(2, [
      { type: 'compactions_replace', compactions: [] },
      {
        type: 'transcript_append',
        items: [{
          type: 'compaction',
          seq: 5,
          ts: '2026-09-03T00:00:01Z',
          operation_id: active.id,
          context_id: '00000000-0000-0000-0000-000000000011',
          run_id: '00000000-0000-0000-0000-000000000012',
          outcome: 'finished',
          range_start: 2,
          range_end: 8,
          compacted_count: 7,
          before_tokens: 10_000,
          after_tokens: 2_000,
          summary: 'complete',
        }],
      },
    ]))]))
    expect(store.current.projection.compactions).toEqual([])
    expect(store.current.projection.transcript?.at(-1)).toMatchObject({
      type: 'compaction',
      operation_id: active.id,
      outcome: 'finished',
      compacted_count: 7,
    })
  })

  test('reconciles captured steering and consumption atomically across snapshot resume', () => {
    const store = new SessionStore(snapshot())
    const message: TranscriptItem = {
      type: 'message',
      seq: 5,
      ts: '2026-09-04T00:00:00Z',
      run_id: 'run-1',
      message: {
        role: 'user',
        origin: 'interjection',
        turn_id: 'turn-1',
        parts: [{ type: 'text', text: 'captured steering' }],
      },
    }
    const interjection: InterjectionProjection = {
      id: 'interjection-1',
      turn_id: 'turn-1',
      run_id: 'run-1',
      text: 'steering',
      state: 'injected',
      level: 'nudge',
      created_at: '2026-09-04T00:00:00Z',
      source: { type: 'user' },
    }
    const observations: unknown[] = []
    store.subscribe((current) => observations.push({
      messages: current.projection.transcript?.length,
      state: current.projection.interactions?.interjections?.[0]?.state,
    }))
    store.applyUpdates(page([event(1, delta(1, [
      { type: 'transcript_append', items: [message] },
      {
        type: 'interaction_upsert',
        interaction: { type: 'interjection', interjection },
      },
    ]))]))
    expect(observations).toEqual([{ messages: 1, state: 'injected' }])
    expect(store.current.projection.transcript).toEqual([message])

    const resumed = new SessionStore(store.current)
    const compacted = page([event(2, delta(2, [{ type: 'transcript_replace', items: [] }]))])
    store.applyUpdates(compacted)
    resumed.applyUpdates(compacted)
    expect(resumed.current).toEqual(store.current)
    expect(store.current.projection.interactions?.interjections).toEqual([interjection])
  })

  test('applies ordered deltas and publishes one immutable view', () => {
    const store = new SessionStore(snapshot())
    const views: number[] = []
    store.subscribe((current) => views.push(current.cursor))
    const change: ProjectionChange = {
      type: 'run_upsert',
      run: {
        id: 'run-1',
        flow_name: 'agent',
        state: 'running',
        started_at: '2026-09-03T00:00:00Z',
      },
    }
    const response = page([
      event(1, delta(1, [{ type: 'lifecycle_set', lifecycle: 'active' }])),
      event(2, delta(2, [change])),
    ])

    expect(store.applyUpdates(response)).toEqual({ events: 2, signals: [], hasMore: false })
    expect(store.current.cursor).toBe(2)
    expect(store.current.projection.revision).toBe(2)
    expect(store.current.projection.lifecycle).toBe('active')
    expect(store.current.projection.runs?.[0]?.id).toBe('run-1')
    expect(views).toEqual([2])

    change.run.state = 'failed'
    expect(store.current.projection.runs?.[0]?.state).toBe('running')
    expect(Object.isFrozen(store.current.projection.runs?.[0])).toBeTrue()
  })

  test('shares unchanged projection branches across updates', () => {
    const store = new SessionStore(snapshot())
    const previous = store.current

    store.applyUpdates(page([event(1, delta(1, [{
      type: 'run_upsert',
      run: {
        id: 'run-1',
        flow_name: 'agent',
        state: 'running',
        started_at: '2026-09-03T00:00:00Z',
      },
    }]))]))

    expect(store.current).not.toBe(previous)
    expect(store.current.projection).not.toBe(previous.projection)
    expect(store.current.projection.runs).not.toBe(previous.projection.runs)
    expect(store.current.projection.metadata).toBe(previous.projection.metadata)
    expect(store.current.projection.transcript).toBe(previous.projection.transcript)
    expect(store.current.projection.resources).toBe(previous.projection.resources)
  })

  test('updates and removes workflows without replacing their siblings', () => {
    const store = new SessionStore(snapshot({
      projection: {
        revision: 0,
        lifecycle: 'idle',
        metadata: { id: sessionId, title: 'initial' },
        transcript: [],
        runs: [],
        resources: [],
        workflows: [
          { turn_id: 'turn-1', roots: [] },
          { turn_id: 'turn-2', roots: [] },
        ],
      },
    }))
    const updated = {
      turn_id: 'turn-1',
      roots: [],
      marker: 'updated',
    }

    store.applyUpdates(page([event(1, delta(1, [
      { type: 'workflow_upsert', workflow: updated },
    ]))]))
    updated.marker = 'mutated outside the store'
    expect(store.current.projection.workflows).toEqual([
      { turn_id: 'turn-1', roots: [], marker: 'updated' },
      { turn_id: 'turn-2', roots: [] },
    ])

    store.applyUpdates(page([event(2, delta(2, [
      { type: 'workflow_remove', turn_id: 'turn-1' },
    ]))]))
    expect(store.current.projection.workflows).toEqual([
      { turn_id: 'turn-2', roots: [] },
    ])
  })

  test('deduplicates replayed events without notifying subscribers', () => {
    const store = new SessionStore(snapshot({ cursor: 2 }))
    let notifications = 0
    store.subscribe(() => notifications++)

    const result = store.applyUpdates(
      page([event(1, { type: 'heartbeat' }), event(2, { type: 'heartbeat' })], {
        next_cursor: 2,
      }),
    )

    expect(result.events).toBe(0)
    expect(notifications).toBe(0)
    expect(store.current.cursor).toBe(2)
  })

  test('rolls back an entire page and its signals when a later delta is invalid', () => {
    const store = new SessionStore(snapshot())
    const signals: string[] = []
    store.subscribeSignals((signal) => signals.push(signal.type))
    const response = page([
      event(1, {
        type: 'signal',
        signal: { type: 'llm_text', run_id: 'run-1', text: 'partial' },
      }),
      event(2, {
        type: 'projection_delta',
        delta: {
          base_revision: 7,
          revision: 8,
          changes: [{ type: 'lifecycle_set', lifecycle: 'active' }],
        },
      }),
    ])

    expect(() => store.applyUpdates(response)).toThrow(SessionReconcileError)
    expect(store.current.cursor).toBe(0)
    expect(store.current.projection.lifecycle).toBe('idle')
    expect(signals).toEqual([])
  })

  test('rejects a paginated response that cannot make progress', () => {
    const store = new SessionStore(snapshot())

    expect(() => store.applyUpdates(page([], { has_more: true }))).toThrow(
      SessionReconcileError,
    )
    expect(store.current.cursor).toBe(0)
  })

  test('classifies cursor gaps and explicit resync requests for snapshot recovery', () => {
    const store = new SessionStore(snapshot())
    try {
      store.applyEvent(event(2, { type: 'heartbeat' }))
      throw new Error('expected cursor gap')
    } catch (error) {
      expect(error).toBeInstanceOf(SessionReconcileError)
      expect((error as SessionReconcileError).code).toBe('cursor_gap')
      expect((error as SessionReconcileError).requiresSnapshot).toBeTrue()
    }

    try {
      store.applyEvent(
        event(99, {
          type: 'resync_required',
          gap: {
            available_from: 20,
            requested_after: 0,
            snapshot_revision: 4,
            reason: 'retention gap',
          },
        }),
      )
      throw new Error('expected resync request')
    } catch (error) {
      expect((error as SessionReconcileError).code).toBe('resync_required')
      expect((error as SessionReconcileError).requiresSnapshot).toBeTrue()
    }
  })

  test('rejects wrong generation and session without mutating the view', () => {
    const store = new SessionStore(snapshot())
    expect(() =>
      store.applyUpdates(page([], { daemon_generation: 'generation-2' })),
    ).toThrow(SessionReconcileError)
    expect(() =>
      store.applyEvent(event(1, { type: 'heartbeat' }, { session_id: 'other-session' })),
    ).toThrow(SessionReconcileError)
    expect(() =>
      store.replace(
        snapshot({
          projection: {
            revision: 0,
            lifecycle: 'idle',
            metadata: { id: 'other-session' },
          },
        }),
        generation,
      ),
    ).toThrow(SessionReconcileError)
    expect(store.current.cursor).toBe(0)
  })

  test('subscriber failures cannot roll back state or starve sibling subscribers', () => {
    const errors: unknown[] = []
    const store = new SessionStore(snapshot(), { onSubscriberError: (error) => errors.push(error) })
    const cursors: number[] = []
    store.subscribe(() => {
      throw new Error('broken subscriber')
    })
    store.subscribe((current) => cursors.push(current.cursor))

    store.applyEvent(event(1, { type: 'heartbeat' }))

    expect(store.current.cursor).toBe(1)
    expect(cursors).toEqual([1])
    expect(errors).toHaveLength(1)
  })
})
