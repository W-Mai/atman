import { ClientError, RemoteError } from "./errors.ts";
import type { CommandOutcome } from "./errors.ts";
import { cursorId, durableCursor, EventCursor } from "./cursor.ts";
import type { DurableBatch, DurableCursor, FlowSource } from "./cursor.ts";
import { parseSse } from "./sse.ts";
import type { SseEvent } from "./sse.ts";

export interface ContractRevision {
  readonly protocol_revision: number;
  readonly schema_revision: number;
}

export interface CapabilityRequirement {
  readonly name: string;
  readonly revision: number;
  readonly request_schema?: string;
  readonly response_schema?: string;
}

/** Business descriptors bind exported method facts to their typed schema decoders. */
export interface RpcMethod<Parameters, Result> extends CapabilityRequirement {
  readonly operation: "query" | "command";
  readonly decode: (value: unknown) => Result;
  readonly validate?: (parameters: Parameters) => void;
  readonly validateResult?: (parameters: Parameters, result: Result) => void;
}

export function defineMethod<Parameters, Result>(
  method: RpcMethod<Parameters, Result>,
): Readonly<RpcMethod<Parameters, Result>> {
  requirement(method);
  if (!["query", "command"].includes(method.operation) || typeof method.decode !== "function") {
    throw new ClientError("request", "Method descriptor is invalid", "not_sent");
  }
  return Object.freeze({ ...method });
}

export interface CapabilityCatalog extends ContractRevision {
  readonly host_generation: string;
  readonly methods: Readonly<Record<string, {
    readonly revision: number;
    readonly request_schema?: string;
    readonly response_schema?: string;
  }>>;
}

export interface RequestOptions {
  readonly signal?: AbortSignal;
}

export type Fetch = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;

export interface HttpTransportOptions {
  readonly fetch?: Fetch;
  readonly requestTimeoutMs?: number;
  readonly maxResponseBytes?: number;
}

export interface HttpClientOptions extends HttpTransportOptions {
  readonly endpoint: string;
  readonly token: string;
  readonly contract: ContractRevision;
}

export function transportSettings(options: HttpTransportOptions) {
  const timeout = options.requestTimeoutMs ?? 30_000;
  const maxBytes = options.maxResponseBytes ?? 16 * 1024 * 1024;
  if (!Number.isSafeInteger(timeout) || timeout < 1 || timeout > 0x7fff_ffff) {
    throw new ClientError("request", "Request timeout must be a positive bounded integer", "not_sent");
  }
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 16 * 1024 * 1024) {
    throw new ClientError("request", "Response byte budget must be between one byte and 16 MiB", "not_sent");
  }
  return { fetch: options.fetch ?? globalThis.fetch.bind(globalThis), timeout, maxBytes };
}

function object(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ClientError("protocol", "Expected a JSON object");
  }
  return value as Record<string, unknown>;
}

function revision(value: unknown): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 1 || value > 0xffff_ffff) {
    throw new ClientError("capability", "Expected a positive protocol revision");
  }
  return value;
}

function requirement(value: CapabilityRequirement): void {
  if (typeof value.name !== "string" || value.name.length === 0) {
    throw new ClientError("request", "Method name is empty", "not_sent");
  }
  revision(value.revision);
}

function decodeCapabilities(value: unknown): CapabilityCatalog {
  const source = object(value);
  const methods = Object.create(null) as Record<string, {
    revision: number;
    request_schema?: string;
    response_schema?: string;
  }>;
  for (const [name, raw] of Object.entries(object(source.methods))) {
    const entry = object(raw);
    const method: { revision: number; request_schema?: string; response_schema?: string } = {
      revision: revision(entry.revision),
    };
    for (const field of ["request_schema", "response_schema"] as const) {
      if (entry[field] !== undefined) {
        if (typeof entry[field] !== "string") {
          throw new ClientError("capability", "Method schema identity is invalid");
        }
        method[field] = entry[field];
      }
    }
    methods[name] = Object.freeze(method);
  }
  if (typeof source.host_generation !== "string" || source.host_generation.length === 0) {
    throw new ClientError("capability", "Host generation is absent");
  }
  return Object.freeze({
    protocol_revision: revision(source.protocol_revision),
    schema_revision: revision(source.schema_revision),
    host_generation: source.host_generation,
    methods: Object.freeze(methods),
  });
}

function checkAborted(signal?: AbortSignal): void {
  if (signal?.aborted) throw new ClientError("aborted", "Request was cancelled before dispatch", "not_sent");
}

function captureObservation(options: RequestOptions): RequestOptions {
  const signal = options.signal;
  return signal === undefined ? Object.freeze({}) : Object.freeze({ signal });
}

function captureParameters<Parameters>(parameters: Parameters): Parameters {
  if (parameters === undefined) return parameters;
  try {
    const encoded = JSON.stringify(parameters);
    if (encoded === undefined) throw new Error("not JSON");
    return JSON.parse(encoded) as Parameters;
  } catch {
    throw new ClientError("request", "Request is not JSON serializable", "not_sent");
  }
}

function waitWithAbort<T>(pending: Promise<T>, signal?: AbortSignal): Promise<T> {
  checkAborted(signal);
  if (signal === undefined) return pending;
  return new Promise<T>((resolve, reject) => {
    const stop = () => {
      signal.removeEventListener("abort", stop);
      reject(new ClientError("aborted", "Request was cancelled before dispatch", "not_sent"));
    };
    signal.addEventListener("abort", stop, { once: true });
    pending.then(
      (value) => { signal.removeEventListener("abort", stop); resolve(value); },
      (error: unknown) => { signal.removeEventListener("abort", stop); reject(error); },
    );
  });
}

function requestLease(signal: AbortSignal | undefined, timeoutMs: number) {
  const controller = new AbortController();
  const stop = () => controller.abort();
  let timedOut = false;
  if (signal?.aborted) controller.abort();
  else signal?.addEventListener("abort", stop, { once: true });
  let timer: ReturnType<typeof setTimeout> | undefined = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);
  const clear = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  };
  return {
    signal: controller.signal,
    abort: stop,
    get timedOut() { return timedOut; },
    clearTimeout: clear,
    dispose() { clear(); signal?.removeEventListener("abort", stop); },
  };
}

async function readJson(response: Response, maximum: number): Promise<unknown> {
  if (response.body === null) throw new ClientError("protocol", "JSON response body is absent");
  const reader = response.body.getReader();
  let buffer = new Uint8Array(Math.min(4096, maximum));
  let length = 0;
  let finished = false;
  try {
    while (true) {
      const next = await reader.read();
      if (next.done) { finished = true; break; }
      const needed = length + next.value.byteLength;
      if (needed > maximum) throw new ClientError("limit", "JSON response exceeds its byte budget");
      if (needed > buffer.length) {
        const grown = new Uint8Array(Math.min(maximum, Math.max(needed, buffer.length * 2)));
        grown.set(buffer.subarray(0, length));
        buffer = grown;
      }
      buffer.set(next.value, length);
      length = needed;
    }
    try {
      return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(buffer.subarray(0, length))) as unknown;
    } catch {
      throw new ClientError("protocol", "Response is not valid UTF-8 JSON");
    }
  } finally {
    if (!finished) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

/** Send bounded JSON over the same bearer transport used by RPC and device authentication. */
export async function requestJson(
  transport: ReturnType<typeof transportSettings>,
  url: string,
  init: Pick<RequestInit, "method" | "headers" | "body">,
  options: RequestOptions,
  outcome: CommandOutcome,
): Promise<unknown> {
  checkAborted(options.signal);
  const lease = requestLease(options.signal, transport.timeout);
  try {
    const response = await transport.fetch(url, {
      ...init, signal: lease.signal, credentials: "omit", redirect: "error",
      referrerPolicy: "no-referrer", cache: "no-store",
    });
    if (lease.signal.aborted || !response.ok) {
      await response.body?.cancel().catch(() => {});
      if (lease.signal.aborted) throw new ClientError("aborted", "Request observation was cancelled", outcome);
      throw new ClientError("http", `Request failed with HTTP ${response.status}`, outcome, response.status);
    }
    const raw = await readJson(response, transport.maxBytes);
    if (lease.signal.aborted) throw new ClientError("aborted", "Request observation was cancelled", outcome);
    return raw;
  } catch (error) {
    if (lease.timedOut) throw new ClientError("timeout", "Request deadline expired", outcome);
    if (lease.signal.aborted) throw new ClientError("aborted", "Request observation was cancelled", outcome);
    if (error instanceof ClientError) throw new ClientError(error.kind, error.message, outcome, error.status);
    throw new ClientError("transport", "HTTP request failed", outcome);
  } finally { lease.dispose(); }
}

function decodeEnvelope(value: unknown, id: string): unknown {
  const envelope = object(value);
  if (envelope.jsonrpc !== "2.0" || envelope.id !== id) {
    throw new ClientError("protocol", "RPC response version or identity differs from its request");
  }
  const result = Object.hasOwn(envelope, "result");
  const error = Object.hasOwn(envelope, "error");
  if (result === error) throw new ClientError("protocol", "RPC response must have one result or error");
  if (error) {
    const remote = object(envelope.error);
    if (
      typeof remote.code !== "number" || !Number.isInteger(remote.code)
      || remote.code < -0x8000_0000 || remote.code > 0x7fff_ffff
      || typeof remote.message !== "string"
    ) throw new ClientError("protocol", "RPC error body is invalid");
    throw new RemoteError(remote.code, remote.message, remote.data);
  }
  return envelope.result;
}

export interface FlowEventsRequest {
  readonly flow: FlowSource;
  readonly after?: DurableCursor;
}

export interface FlowEventsContract<Batch extends DurableBatch> extends CapabilityRequirement {
  readonly decodeBatch: (value: unknown) => Batch;
}

interface EventConnection {
  readonly events: AsyncGenerator<SseEvent>;
  close(): Promise<void>;
}

/** An HTTP transport with explicit descriptors and no automatic command retry. */
export class HttpClient {
  readonly #rpc: URL;
  readonly #events: URL;
  readonly #token: string;
  readonly #contract: ContractRevision;
  readonly #transport: ReturnType<typeof transportSettings>;
  #nextId = 1;
  #capabilities: CapabilityCatalog | undefined;
  #negotiating: Promise<CapabilityCatalog> | undefined;

  constructor(options: HttpClientOptions) {
    let base: URL;
    try { base = new URL(options.endpoint); }
    catch { throw new ClientError("request", "Endpoint must be an absolute HTTP URL", "not_sent"); }
    if (
      !["http:", "https:"].includes(base.protocol) || base.hostname === ""
      || base.username !== "" || base.password !== "" || base.search !== "" || base.hash !== ""
    ) throw new ClientError("request", "Endpoint must be an HTTP URL without credentials, query or fragment", "not_sent");
    if (!base.pathname.endsWith("/")) base.pathname += "/";
    if (!/^[\x21-\x7e]+$/.test(options.token)) {
      throw new ClientError("request", "Bearer credential must be nonempty printable ASCII without spaces", "not_sent");
    }
    this.#contract = Object.freeze({
      protocol_revision: revision(options.contract.protocol_revision),
      schema_revision: revision(options.contract.schema_revision),
    });
    this.#transport = transportSettings(options);
    this.#rpc = new URL("rpc", base);
    this.#events = new URL("events", base);
    this.#token = options.token;
  }

  async capabilities(options: RequestOptions = {}): Promise<CapabilityCatalog> {
    const observation = captureObservation(options);
    checkAborted(observation.signal);
    if (this.#capabilities !== undefined) return this.#capabilities;
    if (this.#negotiating === undefined) {
      this.#negotiating = this.#call("capabilities", {}, "query")
        .then((value) => {
          const catalog = decodeCapabilities(value);
          if (
            catalog.protocol_revision !== this.#contract.protocol_revision
            || catalog.schema_revision !== this.#contract.schema_revision
          ) throw new ClientError("capability", "Host protocol or schema revision is unsupported", "not_sent");
          this.#capabilities = catalog;
          return catalog;
        })
        .finally(() => { this.#negotiating = undefined; });
    }
    return waitWithAbort(this.#negotiating, observation.signal);
  }

  async request<Parameters, Result>(
    method: RpcMethod<Parameters, Result>,
    parameters: Parameters,
    options: RequestOptions = {},
  ): Promise<Result> {
    const observation = captureObservation(options);
    checkAborted(observation.signal);
    const bound = defineMethod(method);
    const captured = captureParameters(parameters);
    try { bound.validate?.(captured); }
    catch { throw new ClientError("request", "Request parameters are invalid", "not_sent"); }
    await this.#require(bound, observation);
    checkAborted(observation.signal);
    const raw = await this.#call(bound.name, captured, bound.operation, observation);
    try {
      const result = bound.decode(raw);
      bound.validateResult?.(captured, result);
      return result;
    } catch (error) {
      throw new ClientError(
        error instanceof ClientError ? error.kind : "protocol",
        error instanceof ClientError ? error.message : "RPC result does not match its contract",
        bound.operation === "command" ? "unknown" : "not_applicable",
      );
    }
  }

  async subscribeFlowEvents<Batch extends DurableBatch>(
    contract: FlowEventsContract<Batch>,
    request: FlowEventsRequest,
    options: RequestOptions = {},
  ): Promise<FlowEventSubscription<Batch>> {
    const observation = captureObservation(options);
    checkAborted(observation.signal);
    const bound = Object.freeze({ ...contract });
    const cursor = new EventCursor(request.flow, request.after);
    await this.#require(bound, observation);
    checkAborted(observation.signal);
    const url = new URL(this.#events);
    url.searchParams.set("session_id", cursor.source.session_id);
    url.searchParams.set("instance_id", cursor.source.instance_id);
    if (cursor.last !== undefined) url.searchParams.set("after", cursorId(cursor.last));
    const connection = await this.#connectEvents(url, observation);
    return new FlowEventSubscription(cursor, bound.decodeBatch, connection);
  }

  async #require(method: CapabilityRequirement, options: RequestOptions): Promise<void> {
    requirement(method);
    const catalog = await this.capabilities(options);
    const supported = catalog.methods[method.name];
    if (supported === undefined || supported.revision !== method.revision) {
      throw new ClientError("capability", `Method revision is unsupported: ${method.name}`, "not_sent");
    }
    for (const field of ["request_schema", "response_schema"] as const) {
      if (method[field] !== undefined && method[field] !== supported[field]) {
        throw new ClientError("capability", `Method schema identity differs: ${method.name}`, "not_sent");
      }
    }
  }

  #headers(accept: string): Headers {
    return new Headers({ authorization: `Bearer ${this.#token}`, accept });
  }

  async #call(
    method: string,
    parameters: unknown,
    operation: "query" | "command",
    options: RequestOptions = {},
  ): Promise<unknown> {
    checkAborted(options.signal);
    if (!Number.isSafeInteger(this.#nextId)) throw new ClientError("request", "RPC identity is exhausted", "not_sent");
    const id = `rpc:${this.#nextId++}`;
    let body: string;
    try { body = JSON.stringify({ jsonrpc: "2.0", id, method, params: parameters }); }
    catch { throw new ClientError("request", "Request is not JSON serializable", "not_sent"); }
    const headers = this.#headers("application/json");
    headers.set("content-type", "application/json");
    const outcome: CommandOutcome = operation === "command" ? "unknown" : "not_applicable";
    const raw = await requestJson(this.#transport, this.#rpc.href, { method: "POST", headers, body }, options, outcome);
    try {
      return decodeEnvelope(raw, id);
    } catch (error) {
      if (error instanceof ClientError) throw new ClientError(error.kind, error.message, outcome, error.status);
      throw error;
    }
  }

  async #connectEvents(url: URL, options: RequestOptions): Promise<EventConnection> {
    const lease = requestLease(options.signal, this.#transport.timeout);
    try {
      const response = await this.#transport.fetch(url, {
        method: "GET", headers: this.#headers("text/event-stream"), signal: lease.signal,
        credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", cache: "no-store",
      });
      if (lease.signal.aborted) {
        await response.body?.cancel().catch(() => {});
        throw new ClientError("aborted", "Event observation was cancelled");
      }
      if (!response.ok) {
        await response.body?.cancel().catch(() => {});
        throw new ClientError("http", `Event subscription failed with HTTP ${response.status}`, "not_applicable", response.status);
      }
      const media = response.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
      if (media !== "text/event-stream" || response.body === null) {
        await response.body?.cancel().catch(() => {});
        throw new ClientError("protocol", "Expected a text/event-stream response body");
      }
      lease.clearTimeout();
      const body = response.body;
      const cancelUnread = () => {
        if (!body.locked) void body.cancel().catch(() => {});
        lease.dispose();
      };
      lease.signal.addEventListener("abort", cancelUnread, { once: true });
      const parsed = parseSse(body, { signal: lease.signal, maxEventBytes: this.#transport.maxBytes });
      const events = (async function* () {
        try { yield* parsed; }
        finally {
          lease.signal.removeEventListener("abort", cancelUnread);
          lease.dispose();
        }
      })();
      let closing: Promise<void> | undefined;
      return {
        events,
        close() {
          closing ??= (async () => {
            lease.abort();
            try { await events.return(undefined); }
            finally {
              if (!body.locked) await body.cancel().catch(() => {});
              lease.signal.removeEventListener("abort", cancelUnread);
              lease.dispose();
            }
          })();
          return closing;
        },
      };
    } catch (error) {
      lease.dispose();
      if (error instanceof ClientError) throw error;
      if (lease.timedOut) throw new ClientError("timeout", "Event connection deadline expired");
      if (lease.signal.aborted) throw new ClientError("aborted", "Event observation was cancelled");
      throw new ClientError("transport", "Event connection failed");
    }
  }
}

/** A single durable observer; closing it never sends an execution-control command. */
export class FlowEventSubscription<Batch extends DurableBatch> implements AsyncIterableIterator<Batch> {
  readonly #cursor: EventCursor;
  readonly #connection: EventConnection;
  readonly #iterator: AsyncGenerator<Batch>;

  constructor(cursor: EventCursor, decode: (value: unknown) => Batch, connection: EventConnection) {
    this.#cursor = cursor;
    this.#connection = connection;
    this.#iterator = this.#consume(decode);
  }

  get flow(): Readonly<FlowSource> { return this.#cursor.source; }
  get last(): Readonly<DurableCursor> | undefined { return this.#cursor.last; }
  [Symbol.asyncIterator](): AsyncIterableIterator<Batch> { return this; }
  next(): Promise<IteratorResult<Batch>> { return this.#iterator.next(); }
  async return(): Promise<IteratorResult<Batch>> {
    await this.#connection.close();
    return this.#iterator.return(undefined);
  }
  async close(): Promise<void> { await this.return(); }

  async *#consume(decode: (value: unknown) => Batch): AsyncGenerator<Batch> {
    try {
      for await (const event of this.#connection.events) {
        if (event.event !== "flow_update") continue;
        let batch: Batch;
        try { batch = decode(JSON.parse(event.data) as unknown); }
        catch { throw new ClientError("protocol", "Flow event does not match its contract"); }
        const receipt = durableCursor(batch.receipt, this.#cursor.source);
        if (event.id !== cursorId(receipt)) {
          throw new ClientError("source", "Event ID differs from its durable receipt");
        }
        if (this.#cursor.deliver(receipt)) yield batch;
      }
    } catch (error) {
      if (error instanceof ClientError || (error instanceof DOMException && error.name === "AbortError")) throw error;
      throw new ClientError("transport", "Event stream failed");
    } finally { await this.#connection.close(); }
  }
}
