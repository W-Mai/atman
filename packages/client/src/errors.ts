import type { JsonRpcError } from './generated/types.generated'
import type { RpcMethodName } from './generated/methods.generated'

export class AtmanClientError extends Error {
  override readonly name: string = 'AtmanClientError'
}

export class AtmanTransportError extends AtmanClientError {
  override readonly name: string = 'AtmanTransportError'

  constructor(
    message: string,
    readonly retryable: boolean,
    options?: ErrorOptions,
  ) {
    super(message, options)
  }
}

export class AtmanHttpError extends AtmanTransportError {
  override readonly name: string = 'AtmanHttpError'

  constructor(
    readonly status: number,
    readonly body: string,
  ) {
    super(
      `atman daemon returned HTTP ${status}${body ? `: ${body}` : ''}`,
      status === 408 || status === 425 || status === 429 || status >= 500,
    )
  }
}

export class AtmanRpcError extends AtmanClientError {
  override readonly name: string = 'AtmanRpcError'
  readonly code: number
  readonly data: unknown

  constructor(error: JsonRpcError) {
    super(`atman daemon returned JSON-RPC ${error.code}: ${error.message}`)
    this.code = error.code
    this.data = error.data
  }
}

export class AtmanProtocolError extends AtmanClientError {
  override readonly name: string = 'AtmanProtocolError'
}

export type CompatibilityErrorCode =
  | 'protocol'
  | 'snapshot_schema'
  | 'event_schema'
  | 'method'

export class AtmanCompatibilityError extends AtmanProtocolError {
  override readonly name: string = 'AtmanCompatibilityError'

  constructor(
    readonly code: CompatibilityErrorCode,
    message: string,
    readonly details: Readonly<Record<string, unknown>> = {},
  ) {
    super(message)
  }
}

export class UnsupportedMethodError extends AtmanCompatibilityError {
  override readonly name: string = 'UnsupportedMethodError'

  constructor(
    readonly method: RpcMethodName,
    readonly revision: number,
  ) {
    super(
      'method',
      `atman client and daemon are incompatible: method ${method} revision ${revision} is unavailable. Restart or upgrade the daemon from the same atman installation as this client`,
      { method, revision },
    )
  }
}

export type SessionReconcileErrorCode =
  | 'snapshot_schema'
  | 'event_schema'
  | 'daemon_generation'
  | 'session'
  | 'cursor_gap'
  | 'page_cursor'
  | 'revision_base'
  | 'revision_step'
  | 'resync_required'

export class SessionReconcileError extends AtmanProtocolError {
  override readonly name: string = 'SessionReconcileError'

  constructor(
    readonly code: SessionReconcileErrorCode,
    message: string,
    readonly details: Readonly<Record<string, unknown>> = {},
  ) {
    super(message)
  }

  get requiresSnapshot(): boolean {
    return (
      this.code === 'cursor_gap' ||
      this.code === 'page_cursor' ||
      this.code === 'revision_base' ||
      this.code === 'revision_step' ||
      this.code === 'resync_required'
    )
  }
}

export type SessionCommandErrorCode =
  | 'committed_cursor_unavailable'
  | 'command_session'
  | 'command_run'
  | 'command_form'
  | 'command_compact_review'
  | 'command_prompt'
  | 'command_resource'

export class SessionCommandError extends AtmanProtocolError {
  override readonly name: string = 'SessionCommandError'

  constructor(
    readonly code: SessionCommandErrorCode,
    message: string,
    readonly details: Readonly<Record<string, unknown>> = {},
  ) {
    super(message)
  }
}
