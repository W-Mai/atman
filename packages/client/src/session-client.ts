import {
  AtmanTransportError,
  SessionCommandError,
  SessionReconcileError,
} from './errors'
import type {
  AutoNameSessionResponse,
  CancelRunResponse,
  CompactReviewDecision,
  CompactSessionResponse,
  CreatePermissionGroupResponse,
  DaemonGeneration,
  EventCursor,
  FlowRunId,
  FormSubmission,
  GetSessionUpdatesResponse,
  InlineImage,
  InspectResourceResponse,
  InterjectionLevel,
  InterjectSessionResponse,
  ListPermissionRequestsResponse,
  ListResourcesResponse,
  MoveSessionResponse,
  PermissionRpcAction,
  PermissionRpcScope,
  PermissionRpcSelector,
  PromptId,
  ReleaseResourceResponse,
  ReloadSessionMcpResponse,
  RenameSessionResponse,
  ResizeTerminalResourceResponse,
  ResolveCompactReviewResponse,
  ResolvePermissionRequestsResponse,
  ResolvePromptResponse,
  ResourceId,
  RetainResourceResponse,
  SendMessageResponse,
  SessionId,
  SessionSignal,
  SessionSnapshot,
  SetSessionGoalResponse,
  StartRunResponse,
  SubmitFormResponse,
  TerminateResourceResponse,
  TodoMutation,
  TrustProjection,
  UpdateSessionTodosResponse,
  UpdateSessionTrustResponse,
} from './generated/types.generated'
import { SessionStore, type SessionView } from './session-store'
import type { TransportRequestOptions } from './transport'
import type { AtmanClient } from './client'

const DEFAULT_POLL_INTERVAL_MS = 250
const DEFAULT_MIN_RECONNECT_DELAY_MS = 100
const DEFAULT_MAX_RECONNECT_DELAY_MS = 3_000

export type RefreshOutcome =
  | {
      kind: 'applied'
      events: number
      signals: readonly Readonly<SessionSignal>[]
      hasMore: boolean
    }
  | { kind: 'resynced' }
  | { kind: 'reconnected' }

export interface SynchronizeOptions extends TransportRequestOptions {
  pollIntervalMs?: number
  minReconnectDelayMs?: number
  maxReconnectDelayMs?: number
}

export interface MessageOptions extends TransportRequestOptions {
  reasoning?: string | null
  images?: readonly InlineImage[]
}

export interface StartRunOptions extends MessageOptions {
  args?: Readonly<Record<string, unknown>>
  flowName?: string
}

export interface InterjectOptions extends TransportRequestOptions {
  redirectTarget?: string | null
}

export interface ResolvePermissionsOptions extends TransportRequestOptions {
  scope?: PermissionRpcScope | null
  reason?: string | null
}

export class SessionClient {
  readonly #client: AtmanClient
  readonly #sessionId: SessionId
  readonly #store: SessionStore
  #operationTail: Promise<void> = Promise.resolve()

  private constructor(client: AtmanClient, sessionId: SessionId, snapshot: SessionSnapshot) {
    this.#client = client
    this.#sessionId = sessionId
    this.#store = new SessionStore(snapshot, {
      expectedGeneration: client.capabilities.daemon_generation,
      expectedSession: sessionId,
    })
  }

  static async attach(
    client: AtmanClient,
    sessionId: SessionId,
    options: TransportRequestOptions = {},
  ): Promise<SessionClient> {
    const snapshot = await client.call(
      'session.get_snapshot',
      { session_id: sessionId },
      options,
    )
    return new SessionClient(client, sessionId, snapshot)
  }

  get sessionId(): SessionId {
    return this.#sessionId
  }

  get current(): SessionView {
    return this.#store.current
  }

  subscribe(listener: Parameters<SessionStore['subscribe']>[0]): () => void {
    return this.#store.subscribe(listener)
  }

  subscribeSignals(listener: Parameters<SessionStore['subscribeSignals']>[0]): () => void {
    return this.#store.subscribeSignals(listener)
  }

  async refresh(options: TransportRequestOptions = {}): Promise<RefreshOutcome> {
    return this.#exclusive(() => this.#refresh(options))
  }

  async applyEvent(
    event: Parameters<SessionStore['applyEvent']>[0],
    options: TransportRequestOptions = {},
  ): Promise<RefreshOutcome> {
    return this.#exclusive(() =>
      this.#reconcile(
        {
          daemon_generation: event.daemon_generation,
          next_cursor: event.cursor,
          events: [event],
          has_more: false,
        },
        options,
      ),
    )
  }

  async refreshUntilCurrent(
    options: TransportRequestOptions = {},
  ): Promise<RefreshOutcome> {
    let events = 0
    const signals: Readonly<SessionSignal>[] = []
    while (true) {
      const outcome = await this.refresh(options)
      if (outcome.kind !== 'applied') {
        return outcome
      }
      events += outcome.events
      signals.push(...outcome.signals)
      if (!outcome.hasMore) {
        return { kind: 'applied', events, signals, hasMore: false }
      }
    }
  }

  async sendMessage(
    text: string,
    options: MessageOptions = {},
  ): Promise<SendMessageResponse> {
    const response = await this.#client.command(
      'session.send_message',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        text,
        ...(options.reasoning !== undefined ? { reasoning: options.reasoning } : {}),
        ...(options.images ? { images: [...options.images] } : {}),
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async startRun(
    flowPath: string,
    options: StartRunOptions = {},
  ): Promise<StartRunResponse> {
    const response = await this.#client.command(
      'run.start',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        flow_path: flowPath,
        ...(options.flowName !== undefined ? { flow_name: options.flowName } : {}),
        ...(options.args ? { args: { ...options.args } } : {}),
        ...(options.reasoning !== undefined ? { reasoning: options.reasoning } : {}),
        ...(options.images ? { images: [...options.images] } : {}),
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async rename(
    title: string | null,
    options: TransportRequestOptions = {},
  ): Promise<RenameSessionResponse> {
    const response = await this.#client.command(
      'rename_session',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        title,
      },
      options,
    )
    this.#validateSession(response.session.id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async autoName(
    options: TransportRequestOptions = {},
  ): Promise<AutoNameSessionResponse> {
    const response = await this.#client.command(
      'session.auto_name',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
      },
      options,
    )
    this.#validateSession(response.session.id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async moveTo(
    projectRoot: string,
    options: TransportRequestOptions = {},
  ): Promise<MoveSessionResponse> {
    const response = await this.#client.command(
      'session.move',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        project_root: projectRoot,
      },
      options,
    )
    this.#validateSession(response.session.id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async setGoal(
    goal: string | null,
    options: TransportRequestOptions = {},
  ): Promise<SetSessionGoalResponse> {
    const response = await this.#client.command(
      'session.set_goal',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        goal,
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async updateTodos(
    mutation: TodoMutation,
    options: TransportRequestOptions = {},
  ): Promise<UpdateSessionTodosResponse> {
    const response = await this.#client.command(
      'session.update_todos',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        mutation,
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async updateTrust(
    trust: TrustProjection,
    options: TransportRequestOptions = {},
  ): Promise<UpdateSessionTrustResponse> {
    const response = await this.#client.command(
      'session.update_trust',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        trust,
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async compact(
    options: TransportRequestOptions = {},
  ): Promise<CompactSessionResponse> {
    const response = await this.#client.command(
      'session.compact',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async reloadMcp(
    options: TransportRequestOptions = {},
  ): Promise<ReloadSessionMcpResponse> {
    const response = await this.#client.command(
      'session.reload_mcp',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async interject(
    runId: FlowRunId,
    text: string,
    level: InterjectionLevel,
    options: InterjectOptions = {},
  ): Promise<InterjectSessionResponse> {
    const response = await this.#client.command(
      'session.interject',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        run_id: runId,
        text,
        level,
        ...(options.redirectTarget !== undefined
          ? { redirect_target: options.redirectTarget }
          : {}),
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateRun(response.run_id, runId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async cancelRun(
    runId: FlowRunId,
    options: TransportRequestOptions = {},
  ): Promise<CancelRunResponse> {
    const response = await this.#client.command(
      'cancel_run',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        run_id: runId,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateRun(response.run_id, runId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async submitForm(
    formId: string,
    submission: FormSubmission,
    options: TransportRequestOptions = {},
  ): Promise<SubmitFormResponse> {
    const response = await this.#client.command(
      'form.submit',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        form_id: formId,
        submission,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_form', 'form', response.form_id, formId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async resolvePrompt(
    promptId: PromptId,
    answer: unknown,
    options: TransportRequestOptions = {},
  ): Promise<ResolvePromptResponse> {
    const response = await this.#client.command(
      'resolve_prompt',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        prompt_id: promptId,
        answer,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_prompt', 'prompt', response.prompt_id, promptId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async resolveCompactReview(
    reviewId: string,
    decision: CompactReviewDecision,
    options: TransportRequestOptions = {},
  ): Promise<ResolveCompactReviewResponse> {
    const response = await this.#client.command(
      'compact_review.resolve',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        review_id: reviewId,
        decision,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget(
      'command_compact_review',
      'compact review',
      response.review_id,
      reviewId,
    )
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async listPermissions(
    options: TransportRequestOptions = {},
  ): Promise<ListPermissionRequestsResponse> {
    const response = await this.#client.call(
      'list_permission_requests',
      { session_id: this.#sessionId },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async createPermissionGroup(
    requestIds: readonly string[],
    expectedRequestRevisions: Readonly<Record<string, number>>,
    label: string,
    options: TransportRequestOptions = {},
  ): Promise<CreatePermissionGroupResponse> {
    const response = await this.#client.command(
      'create_permission_group',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        request_ids: [...requestIds],
        expected_request_revisions: { ...expectedRequestRevisions },
        label,
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async resolvePermissions(
    selector: PermissionRpcSelector,
    action: PermissionRpcAction,
    options: ResolvePermissionsOptions = {},
  ): Promise<ResolvePermissionRequestsResponse> {
    const response = await this.#client.command(
      'resolve_permission_requests',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        selector,
        action,
        ...(options.scope !== undefined ? { scope: options.scope } : {}),
        ...(options.reason !== undefined ? { reason: options.reason } : {}),
      },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async listResources(
    options: TransportRequestOptions = {},
  ): Promise<ListResourcesResponse> {
    const response = await this.#client.call(
      'resource.list',
      { session_id: this.#sessionId },
      options,
    )
    this.#validateSession(response.session_id)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async inspectResource(
    resourceId: ResourceId,
    options: TransportRequestOptions = {},
  ): Promise<InspectResourceResponse> {
    const response = await this.#client.call(
      'resource.inspect',
      { session_id: this.#sessionId, resource_id: resourceId },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_resource', 'resource', response.resource.id, resourceId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async terminateResource(
    resourceId: ResourceId,
    options: TransportRequestOptions = {},
  ): Promise<TerminateResourceResponse> {
    const response = await this.#client.command(
      'resource.terminate',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        resource_id: resourceId,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_resource', 'resource', response.resource_id, resourceId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async resizeTerminal(
    resourceId: ResourceId,
    rows: number,
    cols: number,
    options: TransportRequestOptions = {},
  ): Promise<ResizeTerminalResourceResponse> {
    const response = await this.#client.command(
      'resource.resize_terminal',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        resource_id: resourceId,
        rows,
        cols,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_resource', 'resource', response.resource_id, resourceId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async retainResource(
    resourceId: ResourceId,
    options: TransportRequestOptions = {},
  ): Promise<RetainResourceResponse> {
    const response = await this.#client.command(
      'resource.retain',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        resource_id: resourceId,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_resource', 'resource', response.resource.id, resourceId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async releaseResource(
    resourceId: ResourceId,
    options: TransportRequestOptions = {},
  ): Promise<ReleaseResourceResponse> {
    const response = await this.#client.command(
      'resource.release',
      {
        request_id: crypto.randomUUID(),
        session_id: this.#sessionId,
        resource_id: resourceId,
      },
      options,
    )
    this.#validateSession(response.session_id)
    this.#validateTarget('command_resource', 'resource', response.resource.id, resourceId)
    await this.#refreshThrough(response.cursor, options)
    return response
  }

  async synchronize(options: SynchronizeOptions = {}): Promise<never> {
    const pollInterval = delayOption(
      options.pollIntervalMs,
      DEFAULT_POLL_INTERVAL_MS,
      'pollIntervalMs',
    )
    const minReconnectDelay = delayOption(
      options.minReconnectDelayMs,
      DEFAULT_MIN_RECONNECT_DELAY_MS,
      'minReconnectDelayMs',
    )
    const maxReconnectDelay = delayOption(
      options.maxReconnectDelayMs,
      DEFAULT_MAX_RECONNECT_DELAY_MS,
      'maxReconnectDelayMs',
    )
    if (maxReconnectDelay < minReconnectDelay) {
      throw new RangeError('maxReconnectDelayMs must be at least minReconnectDelayMs')
    }

    let reconnectDelay = minReconnectDelay
    while (true) {
      throwIfAborted(options.signal)
      try {
        const result = await this.#synchronizeConnection(options)
        if (result === 'reset') {
          reconnectDelay = minReconnectDelay
          continue
        }
        const wait = result === 'polled' ? pollInterval : reconnectDelay
        await abortableDelay(wait, options.signal)
        reconnectDelay =
          result === 'polled'
            ? minReconnectDelay
            : Math.min(reconnectDelay * 2, maxReconnectDelay)
      } catch (error) {
        if (options.signal?.aborted) {
          throw error
        }
        if (!isRetryable(error)) {
          throw error
        }
        await abortableDelay(reconnectDelay, options.signal)
        reconnectDelay = Math.min(reconnectDelay * 2, maxReconnectDelay)
      }
    }
  }

  async #refresh(options: TransportRequestOptions): Promise<RefreshOutcome> {
    const response = await this.#client.call(
      'session.get_updates',
      {
        session_id: this.#sessionId,
        after_cursor: this.#store.current.cursor,
        limit: this.#client.capabilities.limits.max_event_page_size,
      },
      options,
    )
    return this.#reconcile(response, options)
  }

  async #reconcile(
    response: GetSessionUpdatesResponse,
    options: TransportRequestOptions,
  ): Promise<RefreshOutcome> {
    try {
      const applied = this.#store.applyUpdates(response)
      return {
        kind: 'applied',
        events: applied.events,
        signals: applied.signals,
        hasMore: applied.hasMore,
      }
    } catch (error) {
      if (!(error instanceof SessionReconcileError)) {
        throw error
      }
      if (error.code === 'daemon_generation') {
        await this.#reloadSnapshot(receivedGeneration(error), options)
        return { kind: 'reconnected' }
      }
      if (error.requiresSnapshot) {
        await this.#reloadSnapshot(undefined, options)
        return { kind: 'resynced' }
      }
      throw error
    }
  }

  async #reloadSnapshot(
    observedGeneration: DaemonGeneration | undefined,
    options: TransportRequestOptions,
  ): Promise<void> {
    let capabilities = this.#client.capabilities
    if (
      observedGeneration !== undefined &&
      capabilities.daemon_generation !== observedGeneration
    ) {
      capabilities = await this.#client.refreshCapabilities(options)
    }

    let snapshot = await this.#client.call(
      'session.get_snapshot',
      { session_id: this.#sessionId },
      options,
    )
    if (snapshot.daemon_generation !== capabilities.daemon_generation) {
      capabilities = await this.#client.refreshCapabilities(options)
      if (snapshot.daemon_generation !== capabilities.daemon_generation) {
        snapshot = await this.#client.call(
          'session.get_snapshot',
          { session_id: this.#sessionId },
          options,
        )
      }
    }
    this.#store.replace(snapshot, capabilities.daemon_generation)
  }

  async #synchronizeConnection(
    options: TransportRequestOptions,
  ): Promise<'polled' | 'ended' | 'reset'> {
    await this.refreshUntilCurrent(options)
    throwIfAborted(options.signal)
    const events = this.#client.sessionEvents(
      this.#sessionId,
      this.#store.current.cursor,
      options,
    )
    if (!events) {
      return 'polled'
    }
    for await (const event of events) {
      const outcome = await this.applyEvent(event, options)
      if (outcome.kind !== 'applied') {
        return 'reset'
      }
      throwIfAborted(options.signal)
    }
    return 'ended'
  }

  async #refreshThrough(
    target: EventCursor,
    options: TransportRequestOptions,
  ): Promise<void> {
    let stalled = 0
    while (this.#store.current.cursor < target) {
      const before = this.#store.current.cursor
      await this.refresh(options)
      const after = this.#store.current.cursor
      if (after === before) {
        stalled += 1
        if (stalled >= 2) {
          throw new SessionCommandError(
            'committed_cursor_unavailable',
            `daemon acknowledged cursor ${target}, but the session remained at ${after}`,
            { target, current: after },
          )
        }
      } else {
        stalled = 0
      }
    }
  }

  #validateSession(received: SessionId): void {
    if (received !== this.#sessionId) {
      throw new SessionCommandError(
        'command_session',
        `command result belongs to session ${received}, expected ${this.#sessionId}`,
        { expected: this.#sessionId, received },
      )
    }
  }

  #validateRun(received: FlowRunId, expected: FlowRunId): void {
    if (received !== expected) {
      throw new SessionCommandError(
        'command_run',
        `command result belongs to run ${received}, expected ${expected}`,
        { expected, received },
      )
    }
  }

  #validateTarget(
    code:
      | 'command_form'
      | 'command_compact_review'
      | 'command_prompt'
      | 'command_resource',
    target: string,
    received: string,
    expected: string,
  ): void {
    if (received !== expected) {
      throw new SessionCommandError(
        code,
        `command result belongs to ${target} ${received}, expected ${expected}`,
        { expected, received },
      )
    }
  }

  async #exclusive<T>(operation: () => Promise<T>): Promise<T> {
    const previous = this.#operationTail
    let release = () => {}
    this.#operationTail = new Promise<void>((resolve) => {
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

function receivedGeneration(error: SessionReconcileError): DaemonGeneration | undefined {
  const received = error.details.received
  return typeof received === 'string' ? received : undefined
}

function isRetryable(error: unknown): boolean {
  return error instanceof AtmanTransportError && error.retryable
}

function delayOption(value: number | undefined, fallback: number, name: string): number {
  const delay = value ?? fallback
  if (!Number.isFinite(delay) || delay < 0) {
    throw new RangeError(`${name} must be a finite non-negative number`)
  }
  return delay
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
  }
}

function abortableDelay(milliseconds: number, signal: AbortSignal | undefined): Promise<void> {
  throwIfAborted(signal)
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      clearTimeout(timer)
      reject(signal?.reason ?? new DOMException('The operation was aborted', 'AbortError'))
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort)
      resolve()
    }, milliseconds)
    signal?.addEventListener('abort', onAbort, { once: true })
  })
}
