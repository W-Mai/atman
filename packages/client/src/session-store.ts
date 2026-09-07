import { AtmanProtocolError, SessionReconcileError } from './errors'
import { EVENT_SCHEMA_VERSION, SNAPSHOT_SCHEMA_VERSION } from './generated/methods.generated'
import type {
  DaemonGeneration,
  GetSessionUpdatesResponse,
  InteractionItem,
  InteractionTarget,
  ProjectionChange,
  ProjectionDelta,
  ProjectionEventEnvelope,
  ResourceProjection,
  RunProjection,
  SessionId,
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
  boundedTranscript?: boolean
  expectedGeneration?: DaemonGeneration
  expectedSession?: SessionId
  onSubscriberError?: (error: unknown) => void
}

export class SessionStore {
  #snapshot: SessionSnapshot
  #boundedTranscript: boolean
  readonly #listeners = new Set<SessionListener>()
  readonly #signalListeners = new Set<SessionSignalListener>()
  readonly #onSubscriberError: (error: unknown) => void

  constructor(snapshot: SessionSnapshot, options: SessionStoreOptions = {}) {
    const expectedGeneration = options.expectedGeneration ?? snapshot.daemon_generation
    validateSnapshot(snapshot, expectedGeneration)
    if (options.expectedSession) {
      validateSession(snapshot, options.expectedSession)
    }
    this.#snapshot = freezeJson(structuredClone(snapshot))
    this.#boundedTranscript = options.boundedTranscript ?? false
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

  replace(
    snapshot: SessionSnapshot,
    expectedGeneration: DaemonGeneration,
    boundedTranscript = false,
  ): void {
    validateSnapshot(snapshot, expectedGeneration)
    validateSession(snapshot, this.#snapshot.projection.metadata.id)
    const previous = this.#snapshot
    this.#snapshot = freezeJson(structuredClone(snapshot))
    this.#boundedTranscript = boundedTranscript
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
    const next = cloneSnapshotShell(this.#snapshot)
    const signals: SessionSignal[] = []
    let events = 0
    for (const envelope of response.events) {
      if (envelope.cursor <= next.cursor) {
        continue
      }
      applyEnvelope(next, envelope, signals, this.#boundedTranscript)
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
    if (response.has_more && next.cursor === startingCursor) {
      throw reconcileError(
        'page_cursor',
        `session update page declared more events without advancing cursor ${startingCursor}`,
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
  boundedTranscript: boolean,
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
      applyDelta(snapshot.projection, envelope.event.delta, boundedTranscript)
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

function applyDelta(
  projection: SessionProjection,
  delta: ProjectionDelta,
  boundedTranscript: boolean,
): void {
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
    applyChange(projection, change, boundedTranscript)
  }
  projection.revision = delta.revision
}

function applyChange(
  projection: SessionProjection,
  change: ProjectionChange,
  boundedTranscript: boolean,
): void {
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
      projection.transcript = boundedTranscript
        ? replaceBoundedTranscript(projection, change.items)
        : structuredClone(change.items)
      break
    case 'workflow_upsert':
      projection.workflows = upsertByKey(
        projection.workflows ?? [],
        change.workflow,
        (workflow) => workflow.turn_id,
      )
      break
    case 'workflow_remove':
      projection.workflows = (projection.workflows ?? []).filter(
        (workflow) => workflow.turn_id !== change.turn_id,
      )
      break
    case 'compactions_replace':
      projection.compactions = structuredClone(change.compactions)
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
    case 'interaction_upsert':
      upsertInteraction(projection, change.interaction)
      break
    case 'interaction_remove':
      removeInteraction(projection, change.target)
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

function replaceBoundedTranscript(
  projection: SessionProjection,
  replacement: NonNullable<SessionProjection['transcript']> = [],
): NonNullable<SessionProjection['transcript']> {
  const runTurns = new Map(
    (projection.runs ?? [])
      .filter((run) => run.turn_id != null)
      .map((run) => [run.id, run.turn_id as string]),
  )
  const visibleTurns = new Set(
    (projection.transcript ?? [])
      .map((item) => transcriptTurnId(item, runTurns))
      .filter((turn): turn is string => turn !== undefined),
  )
  const minimumSeq = (projection.transcript ?? []).reduce(
    (minimum, item) => Math.min(minimum, item.seq),
    Number.POSITIVE_INFINITY,
  )
  return structuredClone(
    (replacement ?? []).filter((item) => {
      const turn = transcriptTurnId(item, runTurns)
      return turn === undefined
        ? minimumSeq === Number.POSITIVE_INFINITY || item.seq >= minimumSeq
        : visibleTurns.has(turn)
    }),
  )
}

function transcriptTurnId(
  item: NonNullable<SessionProjection['transcript']>[number],
  runTurns: ReadonlyMap<string, string>,
): string | undefined {
  switch (item.type) {
    case 'message':
      return item.message.turn_id
    case 'file_edit':
      return item.turn_id ?? (item.run_id ? runTurns.get(item.run_id) : undefined) ?? undefined
    case 'activity_summary':
      return item.turn_id
    case 'diff':
    case 'compaction':
      return item.run_id ? runTurns.get(item.run_id) : undefined
    case 'mermaid':
    case 'notice':
    case 'extension':
      return undefined
    default:
      assertNever(item)
  }
}

function upsertById<T extends RunProjection | ResourceProjection>(items: T[], item: T): T[] {
  return upsertByKey(items, item, (existing) => existing.id)
}

function upsertByKey<T>(items: T[], item: T, key: (item: T) => string): T[] {
  const next = items.slice()
  const itemKey = key(item)
  const index = next.findIndex((existing) => key(existing) === itemKey)
  if (index === -1) {
    next.push(structuredClone(item))
  } else {
    next[index] = structuredClone(item)
  }
  return next
}

function upsertInteraction(projection: SessionProjection, interaction: InteractionItem): void {
  const current = { ...(projection.interactions ?? {}) }
  switch (interaction.type) {
    case 'prompt':
      current.prompts = upsertByKey(current.prompts ?? [], interaction.prompt, (item) => item.id)
      break
    case 'approval':
      current.approvals = upsertByKey(
        current.approvals ?? [],
        interaction.approval,
        (item) => item.id,
      )
      break
    case 'approval_group':
      current.approval_groups = upsertByKey(
        current.approval_groups ?? [],
        interaction.group,
        (item) => item.id,
      )
      break
    case 'form':
      current.forms = upsertByKey(current.forms ?? [], interaction.form, (item) => item.id)
        .sort((left, right) => left.emitted_at.localeCompare(right.emitted_at))
      break
    case 'compact_review':
      current.compact_reviews = upsertByKey(
        current.compact_reviews ?? [],
        interaction.review,
        (item) => item.id,
      )
      break
    case 'interjection':
      current.interjections = upsertByKey(
        current.interjections ?? [],
        interaction.interjection,
        (item) => item.id,
      ).sort((left, right) => left.created_at.localeCompare(right.created_at))
      break
    default:
      assertNever(interaction)
  }
  projection.interactions = current
}

function removeInteraction(projection: SessionProjection, target: InteractionTarget): void {
  const current = { ...(projection.interactions ?? {}) }
  switch (target.type) {
    case 'prompt':
      current.prompts = (current.prompts ?? []).filter((item) => item.id !== target.prompt_id)
      break
    case 'approval':
      current.approvals = (current.approvals ?? []).filter(
        (item) => item.id !== target.approval_id,
      )
      break
    case 'approval_group':
      current.approval_groups = (current.approval_groups ?? []).filter(
        (item) => item.id !== target.group_id,
      )
      break
    case 'form':
      current.forms = (current.forms ?? []).filter((item) => item.id !== target.form_id)
      break
    case 'compact_review':
      current.compact_reviews = (current.compact_reviews ?? []).filter(
        (item) => item.id !== target.review_id,
      )
      break
    case 'interjection':
      current.interjections = (current.interjections ?? []).filter(
        (item) => item.id !== target.interjection_id,
      )
      break
    default:
      assertNever(target)
  }
  projection.interactions = current
}

function cloneSnapshotShell(snapshot: SessionSnapshot): SessionSnapshot {
  return {
    ...snapshot,
    projection: { ...snapshot.projection },
  }
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
