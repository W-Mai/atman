import { AtmanProtocolError, SessionReconcileError } from './errors'
import { EVENT_SCHEMA_VERSION, SNAPSHOT_SCHEMA_VERSION } from './generated/methods.generated'
import type {
  DaemonGeneration,
  GetSessionUpdatesResponse,
  ProjectionChange,
  ProjectionDelta,
  ProjectionEventEnvelope,
  ResourceProjection,
  RunProjection,
  SessionProjection,
  SessionSignal,
  SessionSnapshot,
} from './generated/types.generated'

export type SessionView = Readonly<SessionSnapshot>
export type SessionListener = (current: SessionView, previous: SessionView) => void
export type SessionSignalListener = (signal: Readonly<SessionSignal>) => void

export interface AppliedSessionUpdates {
  events: number
  signals: readonly Readonly<SessionSignal>[]
  hasMore: boolean
}

export interface SessionStoreOptions {
  expectedGeneration?: DaemonGeneration
  onSubscriberError?: (error: unknown) => void
}

export class SessionStore {
  #snapshot: SessionSnapshot
  readonly #listeners = new Set<SessionListener>()
  readonly #signalListeners = new Set<SessionSignalListener>()
  readonly #onSubscriberError: (error: unknown) => void

  constructor(snapshot: SessionSnapshot, options: SessionStoreOptions = {}) {
    const expectedGeneration = options.expectedGeneration ?? snapshot.daemon_generation
    validateSnapshot(snapshot, expectedGeneration)
    this.#snapshot = freezeJson(structuredClone(snapshot))
    this.#onSubscriberError = options.onSubscriberError ?? reportSubscriberError
  }

  get current(): SessionView {
    return this.#snapshot
  }

  subscribe(listener: SessionListener): () => void {
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }

  subscribeSignals(listener: SessionSignalListener): () => void {
    this.#signalListeners.add(listener)
    return () => this.#signalListeners.delete(listener)
  }

  replace(snapshot: SessionSnapshot, expectedGeneration: DaemonGeneration): void {
    validateSnapshot(snapshot, expectedGeneration)
    validateSession(snapshot, this.#snapshot.projection.metadata.id)
    const previous = this.#snapshot
    this.#snapshot = freezeJson(structuredClone(snapshot))
    this.#publish(previous)
  }

  applyEvent(event: ProjectionEventEnvelope): AppliedSessionUpdates {
    return this.applyUpdates({
      daemon_generation: event.daemon_generation,
      next_cursor: event.cursor,
      events: [event],
      has_more: false,
    })
  }

  applyUpdates(response: GetSessionUpdatesResponse): AppliedSessionUpdates {
    if (response.daemon_generation !== this.#snapshot.daemon_generation) {
      throw reconcileError(
        'daemon_generation',
        `daemon generation changed from ${this.#snapshot.daemon_generation} to ${response.daemon_generation}`,
        {
          expected: this.#snapshot.daemon_generation,
          received: response.daemon_generation,
        },
      )
    }
    if (response.resync_required) {
      throw reconcileError(
        'resync_required',
        `daemon requires a fresh session snapshot: ${response.resync_required.reason}`,
        { gap: response.resync_required },
      )
    }

    const startingCursor = this.#snapshot.cursor
    const next = structuredClone(this.#snapshot)
    const signals: SessionSignal[] = []
    let events = 0
    for (const envelope of response.events) {
      if (envelope.cursor <= next.cursor) {
        continue
      }
      applyEnvelope(next, envelope, signals)
      events += 1
    }
    if (
      response.next_cursor > next.cursor ||
      (response.next_cursor > startingCursor && response.next_cursor !== next.cursor)
    ) {
      throw reconcileError(
        'page_cursor',
        `session update page ended at cursor ${next.cursor}, but declared ${response.next_cursor}`,
        { actual: next.cursor, declared: response.next_cursor },
      )
    }

    const publishedSignals = signals.map((signal) => freezeJson(structuredClone(signal)))
    if (events > 0) {
      const previous = this.#snapshot
      this.#snapshot = freezeJson(next)
      this.#publish(previous)
    }
    for (const signal of publishedSignals) {
      for (const listener of this.#signalListeners) {
        try {
          listener(signal)
        } catch (error) {
          this.#onSubscriberError(error)
        }
      }
    }
    return { events, signals: publishedSignals, hasMore: response.has_more }
  }

  #publish(previous: SessionView): void {
    for (const listener of this.#listeners) {
      try {
        listener(this.#snapshot, previous)
      } catch (error) {
        this.#onSubscriberError(error)
      }
    }
  }
}

function validateSession(snapshot: SessionSnapshot, expectedSession: string): void {
  if (snapshot.projection.metadata.id !== expectedSession) {
    throw reconcileError(
      'session',
      `snapshot belongs to session ${snapshot.projection.metadata.id}, expected ${expectedSession}`,
      { expected: expectedSession, received: snapshot.projection.metadata.id },
    )
  }
}

function validateSnapshot(
  snapshot: SessionSnapshot,
  expectedGeneration: DaemonGeneration,
): void {
  if (snapshot.schema_version !== SNAPSHOT_SCHEMA_VERSION) {
    throw reconcileError(
      'snapshot_schema',
      `snapshot schema version ${snapshot.schema_version} is incompatible with client version ${SNAPSHOT_SCHEMA_VERSION}`,
      { expected: SNAPSHOT_SCHEMA_VERSION, received: snapshot.schema_version },
    )
  }
  if (snapshot.daemon_generation !== expectedGeneration) {
    throw reconcileError(
      'daemon_generation',
      `snapshot belongs to daemon generation ${snapshot.daemon_generation}, expected ${expectedGeneration}`,
      { expected: expectedGeneration, received: snapshot.daemon_generation },
    )
  }
}

function applyEnvelope(
  snapshot: SessionSnapshot,
  envelope: ProjectionEventEnvelope,
  signals: SessionSignal[],
): void {
  if (envelope.schema_version !== EVENT_SCHEMA_VERSION) {
    throw reconcileError(
      'event_schema',
      `projection event schema version ${envelope.schema_version} is incompatible with client version ${EVENT_SCHEMA_VERSION}`,
      { expected: EVENT_SCHEMA_VERSION, received: envelope.schema_version },
    )
  }
  if (envelope.daemon_generation !== snapshot.daemon_generation) {
    throw reconcileError(
      'daemon_generation',
      `projection event belongs to daemon generation ${envelope.daemon_generation}, expected ${snapshot.daemon_generation}`,
      { expected: snapshot.daemon_generation, received: envelope.daemon_generation },
    )
  }
  if (envelope.session_id !== snapshot.projection.metadata.id) {
    throw reconcileError(
      'session',
      `projection event belongs to session ${envelope.session_id}, expected ${snapshot.projection.metadata.id}`,
      { expected: snapshot.projection.metadata.id, received: envelope.session_id },
    )
  }
  if (envelope.event.type === 'resync_required') {
    throw reconcileError(
      'resync_required',
      `daemon requires a fresh session snapshot: ${envelope.event.gap.reason}`,
      { gap: envelope.event.gap },
    )
  }
  const expectedCursor = snapshot.cursor + 1
  if (envelope.cursor !== expectedCursor) {
    throw reconcileError(
      'cursor_gap',
      `session update cursor jumped from ${snapshot.cursor} to ${envelope.cursor}`,
      { current: snapshot.cursor, received: envelope.cursor },
    )
  }

  switch (envelope.event.type) {
    case 'projection_delta':
      applyDelta(snapshot.projection, envelope.event.delta)
      break
    case 'signal':
      signals.push(envelope.event.signal)
      break
    case 'heartbeat':
      break
    default:
      assertNever(envelope.event)
  }
  snapshot.cursor = envelope.cursor
}

function applyDelta(projection: SessionProjection, delta: ProjectionDelta): void {
  if (delta.base_revision !== projection.revision) {
    throw reconcileError(
      'revision_base',
      `projection delta expected revision ${projection.revision}, received base ${delta.base_revision}`,
      { expected: projection.revision, received: delta.base_revision },
    )
  }
  if (delta.revision !== delta.base_revision + 1) {
    throw reconcileError(
      'revision_step',
      `projection delta must advance revision ${delta.base_revision} by one, received ${delta.revision}`,
      { base: delta.base_revision, received: delta.revision },
    )
  }
  for (const change of delta.changes) {
    applyChange(projection, change)
  }
  projection.revision = delta.revision
}

function applyChange(projection: SessionProjection, change: ProjectionChange): void {
  switch (change.type) {
    case 'metadata_set':
      projection.metadata = structuredClone(change.metadata)
      break
    case 'lifecycle_set':
      projection.lifecycle = change.lifecycle
      break
    case 'run_upsert':
      projection.runs = upsertById(projection.runs ?? [], change.run)
      break
    case 'run_remove':
      projection.runs = (projection.runs ?? []).filter((run) => run.id !== change.run_id)
      break
    case 'transcript_append':
      projection.transcript = [
        ...(projection.transcript ?? []),
        ...structuredClone(change.items),
      ]
      break
    case 'transcript_replace':
      projection.transcript = structuredClone(change.items)
      break
    case 'workflows_replace':
      projection.workflows = structuredClone(change.workflows)
      break
    case 'goal_set':
      if (change.goal === undefined) {
        delete projection.goal
      } else {
        projection.goal = change.goal
      }
      break
    case 'todos_replace':
      projection.todos = structuredClone(change.todos)
      break
    case 'plans_replace':
      projection.plans = structuredClone(change.plans)
      break
    case 'context_set':
      projection.context = structuredClone(change.context)
      break
    case 'trust_set':
      projection.trust = structuredClone(change.trust)
      break
    case 'interactions_set':
      projection.interactions = structuredClone(change.interactions)
      break
    case 'resource_upsert':
      projection.resources = upsertById(projection.resources ?? [], change.resource)
      break
    case 'resource_remove':
      projection.resources = (projection.resources ?? []).filter(
        (resource) => resource.id !== change.resource_id,
      )
      break
    case 'usage_set':
      projection.usage = structuredClone(change.usage)
      break
    default:
      assertNever(change)
  }
}

function upsertById<T extends RunProjection | ResourceProjection>(items: T[], item: T): T[] {
  const next = items.map((existing) => structuredClone(existing))
  const index = next.findIndex((existing) => existing.id === item.id)
  if (index === -1) {
    next.push(structuredClone(item))
  } else {
    next[index] = structuredClone(item)
  }
  return next
}

function reconcileError(
  code: ConstructorParameters<typeof SessionReconcileError>[0],
  message: string,
  details: Readonly<Record<string, unknown>>,
): SessionReconcileError {
  return new SessionReconcileError(code, message, details)
}

function freezeJson<T>(value: T): T {
  if (typeof value !== 'object' || value === null || Object.isFrozen(value)) {
    return value
  }
  for (const child of Object.values(value)) {
    freezeJson(child)
  }
  return Object.freeze(value)
}

function reportSubscriberError(error: unknown): void {
  const reporter = (globalThis as { reportError?: (error: unknown) => void }).reportError
  if (reporter) {
    reporter(error)
    return
  }
  queueMicrotask(() => {
    throw error
  })
}

function assertNever(value: never): never {
  throw new AtmanProtocolError(`unsupported projection variant: ${JSON.stringify(value)}`)
}
