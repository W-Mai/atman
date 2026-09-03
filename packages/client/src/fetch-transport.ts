import {
  AtmanClientError,
  AtmanHttpError,
  AtmanProtocolError,
  AtmanTransportError,
} from './errors'
import type { JsonRpcResponse } from './generated/types.generated'
import type { RpcMethodName } from './generated/methods.generated'
import type {
  RpcRequestEnvelope,
  RpcTransport,
  TransportRequestOptions,
} from './transport'

const MAX_ERROR_BODY_LENGTH = 4_096

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
    const headers = new Headers(this.#headers)
    headers.set('accept', 'application/json')
    headers.set('content-type', 'application/json')
    const token =
      typeof this.#token === 'function' ? await this.#token() : this.#token
    if (token) {
      headers.set('authorization', `Bearer ${token}`)
    }

    let response: Response
    try {
      response = await this.#fetch(this.#rpcUrl, {
        method: 'POST',
        headers,
        credentials: this.#credentials,
        body: JSON.stringify(request),
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
