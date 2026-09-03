import {
  AtmanClientError,
  AtmanHttpError,
  AtmanProtocolError,
  AtmanTransportError,
} from './errors'
import type {
  EventCursor,
  JsonRpcResponse,
  ProjectionEventEnvelope,
  SessionId,
} from './generated/types.generated'
import type { RpcMethodName } from './generated/methods.generated'
import type {
  RpcRequestEnvelope,
  RpcTransport,
  TransportRequestOptions,
} from './transport'

const MAX_ERROR_BODY_LENGTH = 4_096
const MAX_SSE_FRAME_BYTES = 16 * 1024 * 1024

export type TokenProvider = string | (() => string | Promise<string>)
export type FetchLike = (
  input: string | URL | Request,
  init?: RequestInit,
) => Promise<Response>

export interface FetchTransportOptions {
  baseUrl: string | URL
  token?: TokenProvider
  credentials?: RequestCredentials
  headers?: HeadersInit
  fetch?: FetchLike
}

export class FetchTransport implements RpcTransport {
  readonly #rpcUrl: URL
  readonly #token: TokenProvider | undefined
  readonly #credentials: RequestCredentials
  readonly #headers: Headers
  readonly #fetch: FetchLike

  constructor(options: FetchTransportOptions) {
    const rpcUrl = new URL(options.baseUrl)
    rpcUrl.pathname = `${rpcUrl.pathname.replace(/\/$/, '')}/rpc`
    rpcUrl.search = ''
    rpcUrl.hash = ''
    this.#rpcUrl = rpcUrl
    this.#token = options.token
    this.#credentials = options.credentials ?? 'same-origin'
    this.#headers = new Headers(options.headers)
    this.#fetch = options.fetch ?? globalThis.fetch
    if (!this.#fetch) {
      throw new AtmanTransportError('fetch is not available in this environment', false)
    }
  }

  get rpcUrl(): URL {
    return new URL(this.#rpcUrl)
  }

  async send<M extends RpcMethodName>(
    request: RpcRequestEnvelope<M>,
    options: TransportRequestOptions = {},
  ): Promise<JsonRpcResponse> {
    const response = await this.#request(
      this.#rpcUrl,
      {
        method: 'POST',
        headers: {
          accept: 'application/json',
          'content-type': 'application/json',
        },
        body: JSON.stringify(request),
      },
      options,
    )

    let body: unknown
    try {
      body = await response.json()
    } catch (error) {
      throw new AtmanProtocolError(`atman daemon returned invalid JSON: ${String(error)}`)
    }
    if (!isJsonRpcResponse(body)) {
      throw new AtmanProtocolError('atman daemon returned an invalid JSON-RPC response')
    }
    return body
  }

  async *sessionEvents(
    sessionId: SessionId,
    afterCursor: EventCursor,
    options: TransportRequestOptions = {},
  ): AsyncIterable<ProjectionEventEnvelope> {
    const url = new URL(this.#rpcUrl)
    url.pathname = url.pathname.replace(/\/rpc$/, '/session-events')
    url.searchParams.set('session_id', sessionId)
    url.searchParams.set('after_cursor', String(afterCursor))
    const response = await this.#request(
      url,
      {
        method: 'GET',
        headers: {
          accept: 'text/event-stream',
          'last-event-id': String(afterCursor),
        },
      },
      options,
    )
    if (!response.body) {
      throw new AtmanProtocolError('atman daemon returned an event stream without a body')
    }

    const reader = response.body.getReader()
    const frames = new SseByteBuffer()
    let completed = false
    try {
      while (true) {
        const { done, value } = await reader.read()
        if (done) {
          completed = true
          break
        }

        let offset = 0
        while (offset < value.length) {
          const available = MAX_SSE_FRAME_BYTES - frames.length
          if (available === 0) {
            throw frameTooLarge()
          }
          const end = Math.min(value.length, offset + available)
          frames.append(value.subarray(offset, end))
          offset = end

          let frame: Uint8Array | undefined
          while ((frame = frames.takeFrame())) {
            const event = parseSseFrame(frame)
            if (event) {
              yield event
            }
          }
          if (frames.length === MAX_SSE_FRAME_BYTES && offset < value.length) {
            throw frameTooLarge()
          }
        }
      }
    } catch (error) {
      if (error instanceof AtmanClientError) {
        throw error
      }
      if (options.signal?.aborted || isAbortError(error)) {
        throw error
      }
      throw new AtmanTransportError('atman daemon event stream failed', true, {
        cause: error,
      })
    } finally {
      if (!completed) {
        try {
          await reader.cancel()
        } catch {
          // The request may already have been cancelled by its AbortSignal.
        }
      }
      reader.releaseLock()
    }
  }

  async #request(
    url: URL,
    init: RequestInit,
    options: TransportRequestOptions,
  ): Promise<Response> {
    const headers = new Headers(this.#headers)
    for (const [name, value] of new Headers(init.headers)) {
      headers.set(name, value)
    }
    const token =
      typeof this.#token === 'function' ? await this.#token() : this.#token
    if (token) {
      headers.set('authorization', `Bearer ${token}`)
    }

    let response: Response
    try {
      response = await this.#fetch(url, {
        ...init,
        headers,
        credentials: this.#credentials,
        ...(options.signal ? { signal: options.signal } : {}),
      })
    } catch (error) {
      if (error instanceof AtmanClientError) {
        throw error
      }
      if (options.signal?.aborted || isAbortError(error)) {
        throw error
      }
      throw new AtmanTransportError('atman daemon request failed', true, {
        cause: error,
      })
    }

    if (!response.ok) {
      const body = (await response.text()).slice(0, MAX_ERROR_BODY_LENGTH)
      throw new AtmanHttpError(response.status, body)
    }
    return response
  }
}

function frameTooLarge(): AtmanProtocolError {
  return new AtmanProtocolError(
    `atman daemon SSE frame exceeds ${MAX_SSE_FRAME_BYTES} bytes`,
  )
}

class SseByteBuffer {
  #buffer = new Uint8Array(8 * 1024)
  #start = 0
  #end = 0
  #scan = 0

  get length(): number {
    return this.#end - this.#start
  }

  append(chunk: Uint8Array): void {
    this.#reserve(chunk.length)
    this.#buffer.set(chunk, this.#end)
    this.#end += chunk.length
  }

  takeFrame(): Uint8Array | undefined {
    const delimiter = this.#findDelimiter()
    if (!delimiter) {
      return undefined
    }
    const frame = this.#buffer.slice(this.#start, delimiter.position)
    this.#start = delimiter.position + delimiter.length
    if (this.#start === this.#end) {
      this.#start = 0
      this.#end = 0
      this.#scan = 0
    } else {
      this.#scan = this.#start
    }
    return frame
  }

  #findDelimiter(): { position: number; length: number } | undefined {
    for (let index = this.#scan; index < this.#end - 1; index += 1) {
      if (this.#buffer[index] === 10 && this.#buffer[index + 1] === 10) {
        return { position: index, length: 2 }
      }
      if (
        index < this.#end - 3 &&
        this.#buffer[index] === 13 &&
        this.#buffer[index + 1] === 10 &&
        this.#buffer[index + 2] === 13 &&
        this.#buffer[index + 3] === 10
      ) {
        return { position: index, length: 4 }
      }
    }
    this.#scan = Math.max(this.#start, this.#end - 3)
    return undefined
  }

  #reserve(additional: number): void {
    if (this.#end + additional <= this.#buffer.length) {
      return
    }
    if (this.length + additional <= this.#buffer.length) {
      const previousStart = this.#start
      const length = this.length
      this.#buffer.copyWithin(0, this.#start, this.#end)
      this.#end = length
      this.#scan = Math.max(0, this.#scan - previousStart)
      this.#start = 0
      return
    }
    let capacity = this.#buffer.length
    while (capacity < this.length + additional) {
      capacity *= 2
    }
    const next = new Uint8Array(capacity)
    next.set(this.#buffer.subarray(this.#start, this.#end))
    const previousStart = this.#start
    this.#end = this.length
    this.#scan = Math.max(0, this.#scan - previousStart)
    this.#start = 0
    this.#buffer = next
  }
}

function parseSseFrame(frame: Uint8Array): ProjectionEventEnvelope | undefined {
  let text: string
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(frame)
  } catch (error) {
    throw new AtmanProtocolError(`atman daemon returned invalid UTF-8 in SSE: ${String(error)}`)
  }
  let eventName: string | undefined
  let eventId: string | undefined
  const data: string[] = []
  for (const rawLine of text.split('\n')) {
    const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine
    if (line.startsWith(':')) {
      continue
    }
    const separator = line.indexOf(':')
    const field = separator === -1 ? line : line.slice(0, separator)
    const rawValue = separator === -1 ? '' : line.slice(separator + 1)
    const value = rawValue.startsWith(' ') ? rawValue.slice(1) : rawValue
    switch (field) {
      case 'event':
        eventName = value
        break
      case 'id':
        eventId = value
        break
      case 'data':
        data.push(value)
        break
    }
  }
  if (data.length === 0 || (eventName && eventName !== 'session_event')) {
    return undefined
  }

  let event: unknown
  try {
    event = JSON.parse(data.join('\n'))
  } catch (error) {
    throw new AtmanProtocolError(`atman daemon returned invalid SSE JSON: ${String(error)}`)
  }
  if (!isProjectionEventEnvelope(event)) {
    throw new AtmanProtocolError('atman daemon returned an invalid projection event envelope')
  }
  const id = Number(eventId)
  if (!eventId || !Number.isSafeInteger(id) || id !== event.cursor) {
    throw new AtmanProtocolError(
      `atman daemon SSE id ${String(eventId)} does not match envelope cursor ${event.cursor}`,
    )
  }
  return event
}

function isProjectionEventEnvelope(value: unknown): value is ProjectionEventEnvelope {
  return (
    typeof value === 'object' &&
    value !== null &&
    'cursor' in value &&
    typeof value.cursor === 'number' &&
    'schema_version' in value &&
    typeof value.schema_version === 'number' &&
    'daemon_generation' in value &&
    typeof value.daemon_generation === 'string' &&
    'session_id' in value &&
    typeof value.session_id === 'string' &&
    'event' in value &&
    typeof value.event === 'object' &&
    value.event !== null
  )
}

function isAbortError(error: unknown): boolean {
  return error instanceof Error && error.name === 'AbortError'
}

function isJsonRpcResponse(value: unknown): value is JsonRpcResponse {
  return (
    typeof value === 'object' &&
    value !== null &&
    'jsonrpc' in value &&
    value.jsonrpc === '2.0' &&
    'id' in value
  )
}
